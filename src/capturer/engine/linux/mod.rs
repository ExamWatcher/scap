use std::{
    mem::size_of,
    sync::{
        atomic::{AtomicBool, AtomicU8},
        mpsc::{self, sync_channel, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

use pipewire as pw;
use pw::{
    context::ContextBox,
    main_loop::MainLoopBox,
    properties::properties,
    spa::{
        self,
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            video::VideoFormat,
            ParamType,
        },
        pod::{Pod, Property},
        sys::{
            spa_buffer, spa_meta_header, SPA_META_Header, SPA_PARAM_META_size, SPA_PARAM_META_type,
        },
        utils::{Direction, SpaTypes},
    },
    stream::{Stream, StreamState},
};

use crate::{
    capturer::{FrameSender, Options},
    frame::{
        BGRAFrame, Frame, VideoFrame, convert_bgrx_to_bgra, convert_rgb_to_bgra,
        convert_rgbx_to_bgra, convert_xbgr_to_bgra,
    },
};

use self::{error::LinCapError, portal::ScreenCastPortal};

mod error;
mod portal;

static CAPTURER_STATE: AtomicU8 = AtomicU8::new(0);
static STREAM_STATE_CHANGED_TO_ERROR: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct ListenerUserData {
    pub tx: FrameSender,
    pub format: spa::param::video::VideoInfoRaw,
}

fn param_changed_callback(
    _stream: &Stream,
    user_data: &mut ListenerUserData,
    id: u32,
    param: Option<&Pod>,
) {
    let Some(param) = param else {
        return;
    };
    if id != pw::spa::param::ParamType::Format.as_raw() {
        return;
    }
    let (media_type, media_subtype) = match pw::spa::param::format_utils::parse_format(param) {
        Ok(v) => v,
        Err(_) => return,
    };

    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return;
    }

    user_data
        .format
        .parse(param)
        // TODO: Tell library user of the error
        .expect("Failed to parse format parameter");
}

fn state_changed_callback(
    _stream: &Stream,
    _user_data: &mut ListenerUserData,
    _old: StreamState,
    new: StreamState,
) {
    if let StreamState::Error(e) = new {
        eprintln!("pipewire: State changed to error({e})");
        STREAM_STATE_CHANGED_TO_ERROR.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

unsafe fn get_timestamp(buffer: *mut spa_buffer) -> i64 {
    // SAFETY: caller guarantees `buffer` points to a valid `spa_buffer`
    // (it comes from `stream.dequeue_raw_buffer()` in `process_callback`).
    unsafe {
        let n_metas = (*buffer).n_metas;
        if n_metas > 0 {
            let mut meta_ptr = (*buffer).metas;
            let metas_end = (*buffer).metas.wrapping_add(n_metas as usize);
            while meta_ptr != metas_end {
                if (*meta_ptr).type_ == SPA_META_Header {
                    let meta_header: &mut spa_meta_header =
                        &mut *((*meta_ptr).data as *mut spa_meta_header);
                    return meta_header.pts;
                }
                meta_ptr = meta_ptr.wrapping_add(1);
            }
            0
        } else {
            0
        }
    }
}

fn process_callback(stream: &Stream, user_data: &mut ListenerUserData) {
    let buffer = unsafe { stream.dequeue_raw_buffer() };
    if !buffer.is_null() {
        'outside: {
            let buffer = unsafe { (*buffer).buffer };
            if buffer.is_null() {
                break 'outside;
            }
            let timestamp = unsafe { get_timestamp(buffer) };

            let n_datas = unsafe { (*buffer).n_datas };
            if n_datas < 1 {
                return;
            }
            let frame_size = user_data.format.size();
            let frame_data: Vec<u8> = unsafe {
                std::slice::from_raw_parts(
                    (*(*buffer).datas).data as *mut u8,
                    (*(*buffer).datas).maxsize as usize,
                )
                .to_vec()
            };

            // `timestamp` (from spa_meta_header.pts) is a PipeWire monotonic
            // nanosecond count since an arbitrary reference — not wall-clock.
            // display_time's SystemTime contract is wall-clock, so we use
            // SystemTime::now() here (matches what the macOS and Windows
            // engines do today).  Relative frame ordering survives via
            // channel-send order; sub-millisecond buffer timing is lost.
            let _ = timestamp; // suppress "unused" warning until we wire pts elsewhere
            let display_time = SystemTime::now();

            // Normalise to BGRA: the negotiated PipeWire layout varies by compositor
            // while consumers ask for one (`output_type: BGRAFrame`). A format outside
            // the negotiated set, or bytes that do not match the dimensions (strided
            // rows), drops the frame — never a panic on the PipeWire thread, and the
            // buffer is still re-queued below via `break 'outside`.
            let bgra = match user_data.format.format() {
                VideoFormat::RGBx => convert_rgbx_to_bgra(frame_data),
                VideoFormat::RGB => convert_rgb_to_bgra(frame_data),
                VideoFormat::xBGR => convert_xbgr_to_bgra(frame_data),
                VideoFormat::BGRx => convert_bgrx_to_bgra(frame_data),
                other => {
                    eprintln!("unsupported pipewire frame format ({other:?}); dropping frame");
                    break 'outside;
                }
            };
            let (width, height) = (frame_size.width as usize, frame_size.height as usize);
            if bgra.len() != width * height * 4 {
                eprintln!("frame bytes do not match dimensions; dropping frame");
                break 'outside;
            }
            let frame = Frame::Video(VideoFrame::BGRA(BGRAFrame {
                display_time,
                width: width as i32,
                height: height as i32,
                data: bgra,
            }));
            // Bounded queue: `try_send` + drop-on-full so a slow consumer
            // can never OOM. PipeWire's `VideoMaxFramerate` is advisory and
            // the compositor may ignore it, so this drop is the real guard.
            // `Full` is silent (expected when behind); `Disconnected` is
            // logged like the old send error was.
            if let Err(mpsc::TrySendError::Disconnected(_)) = user_data.tx.try_send(frame) {
                eprintln!("frame receiver disconnected");
            }
        }
    } else {
        eprintln!("Out of buffers");
    }

    unsafe { stream.queue_raw_buffer(buffer) };
}

// TODO: Format negotiation
fn pipewire_capturer(
    options: Options,
    tx: FrameSender,
    ready_sender: &SyncSender<bool>,
    stream_id: u32,
) -> Result<(), LinCapError> {
    let mainloop = MainLoopBox::new(None)?;
    let context = ContextBox::new(mainloop.loop_(), None)?;
    let core = context.connect(None)?;

    let user_data = ListenerUserData {
        tx,
        format: Default::default(),
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "scap",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(user_data.clone())
        .state_changed(state_changed_callback)
        .param_changed(param_changed_callback)
        .process(process_callback)
        .register()?;

    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::RGB,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::BGRx,
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                // Default
                width: 128,
                height: 128,
            },
            pw::spa::utils::Rectangle {
                // Min
                width: 1,
                height: 1,
            },
            pw::spa::utils::Rectangle {
                // Max
                width: 4096,
                height: 4096,
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoMaxFramerate,
            Fraction,
            pw::spa::utils::Fraction {
                // fps == 0 means "OS default" (matches win/mac guards);
                // never advertise 0/1 to the compositor.
                num: options.fps.max(1),
                denom: 1
            }
        ),
    );

    let metas_obj = pw::spa::pod::object!(
        SpaTypes::ObjectParamMeta,
        ParamType::Meta,
        Property::new(
            SPA_PARAM_META_type,
            pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_META_Header))
        ),
        Property::new(
            SPA_PARAM_META_size,
            pw::spa::pod::Value::Int(size_of::<pw::spa::sys::spa_meta_header>() as i32)
        ),
    );

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();
    let metas_values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(metas_obj),
    )?
    .0
    .into_inner();

    let mut params = [
        pw::spa::pod::Pod::from_bytes(&values).unwrap(),
        pw::spa::pod::Pod::from_bytes(&metas_values).unwrap(),
    ];

    stream.connect(
        Direction::Input,
        Some(stream_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    ready_sender.send(true)?;

    while CAPTURER_STATE.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(10));
    }

    let pw_loop = mainloop.loop_();

    // User has called Capturer::start() and we start the main loop
    while CAPTURER_STATE.load(std::sync::atomic::Ordering::Relaxed) == 1
        && /* If the stream state got changed to `Error`, we exit. TODO: tell user that we exited */
          !STREAM_STATE_CHANGED_TO_ERROR.load(std::sync::atomic::Ordering::Relaxed)
    {
        pw_loop.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
    }

    Ok(())
}

pub struct LinuxCapturer {
    capturer_join_handle: Option<JoinHandle<Result<(), LinCapError>>>,
    // The pipewire stream is deleted when the connection is dropped.
    // That's why we keep it alive
    _connection: dbus::blocking::Connection,
}

impl LinuxCapturer {
    /// Open the portal ScreenCast session and start the PipeWire thread.
    ///
    /// Blocking: `create_stream` shows the system's screen picker and waits for the
    /// user (the portal answers cancel as an error, after its own timeout if ignored).
    /// Every failure — no D-Bus session, unsupported cursor mode, a denied or
    /// cancelled picker, a dead setup thread — is an `Err`, never a panic, so the
    /// caller decides whether and when to re-prompt. Call on a thread that may block;
    /// at most one capturer runs per process (the portal session below is global).
    pub fn new(options: &Options, tx: FrameSender) -> Result<Self, LinCapError> {
        let connection = dbus::blocking::Connection::new_session().map_err(|error| {
            LinCapError::new(format!("could not open a D-Bus session: {error}"))
        })?;
        let stream_id = ScreenCastPortal::new(&connection)
            .show_cursor(options.show_cursor)?
            .create_stream()
            .map_err(|error| {
                LinCapError::new(format!("screen-cast session failed (denied or cancelled?): {error}"))
            })?
            .pw_node_id();

        // TODO: Fix this hack
        let options = options.clone();
        let (ready_sender, ready_recv) = sync_channel(1);
        let capturer_join_handle = std::thread::spawn(move || {
            let res = pipewire_capturer(options, tx, &ready_sender, stream_id);
            if res.is_err() {
                ready_sender.send(false)?;
            }
            res
        });

        let ready = ready_recv.recv().map_err(|_| {
            LinCapError::new("capturer thread died during setup".to_owned())
        })?;
        if !ready {
            return Err(LinCapError::new(
                "pipewire thread reported setup failure".to_owned(),
            ));
        }

        Ok(Self {
            capturer_join_handle: Some(capturer_join_handle),
            _connection: connection,
        })
    }

    pub fn start_capture(&self) {
        CAPTURER_STATE.store(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn stop_capture(&mut self) {
        CAPTURER_STATE.store(2, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.capturer_join_handle.take() {
            if let Err(e) = handle.join().expect("Failed to join capturer thread") {
                eprintln!("Error occured capturing: {e}");
            }
        }
        CAPTURER_STATE.store(0, std::sync::atomic::Ordering::Relaxed);
        STREAM_STATE_CHANGED_TO_ERROR.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

pub fn create_capturer(
    options: &Options,
    tx: FrameSender,
) -> Result<LinuxCapturer, LinCapError> {
    LinuxCapturer::new(options, tx)
}
