use crate::{
    capturer::{Area, FrameSender, Options, Point, Resolution, Size},
    frame::{AudioFormat, AudioFrame, BGRAFrame, Frame, FrameType, VideoFrame},
    targets::{self, Target},
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{cmp, time::Duration};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::Frame as WCFrame,
    graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl},
    monitor::Monitor as WCMonitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings as WCSettings,
    },
    window::Window as WCWindow,
};

#[derive(Debug)]
struct Capturer {
    pub tx: FrameSender,
    pub crop: Option<Area>,
    pub start_time: (Instant, SystemTime),
    /// Minimum time between two queued frames, from `Options.fps`.
    ///
    /// Windows Graphics Capture does not honor `MinimumUpdateInterval`
    /// strictly: on an active screen it delivers far faster than the
    /// requested 1 fps, and every delivery allocates a full frame that is
    /// then dropped on the full queue. Gating here bounds production to what
    /// a 1 Hz consumer can ever take, before any allocation happens.
    pub min_interval: Duration,
    /// When the last frame was accepted for the queue (`None` before the first).
    pub last_sent: Option<Instant>,
}

#[derive(Clone)]
enum Settings {
    Window(WCSettings<FlagStruct, WCWindow>),
    Display(WCSettings<FlagStruct, WCMonitor>),
}

pub struct WCStream {
    settings: Settings,
    capture_control: Option<CaptureControl<Capturer, Box<dyn std::error::Error + Send + Sync>>>,
    audio_stream: Option<AudioStreamHandle>,
}

impl GraphicsCaptureApiHandler for Capturer {
    type Flags = FlagStruct;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            tx: context.flags.tx,
            crop: context.flags.crop,
            start_time: (Instant::now(), SystemTime::now()),
            min_interval: context.flags.min_interval,
            last_sent: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut WCFrame,
        _: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        // Producer-side fps gate: drop frames arriving sooner than the
        // requested interval *before* allocating or converting anything. The
        // OS delivers on content change regardless of `MinimumUpdateInterval`,
        // so without this every delivery pays a full-frame allocation that the
        // bounded queue then discards. The first frame always passes.
        let now = Instant::now();
        let too_soon = match self.last_sent {
            Some(last_sent) => now.duration_since(last_sent) < self.min_interval,
            None => false,
        };
        if too_soon {
            return Ok(());
        }
        self.last_sent = Some(now);
        let display_time = self
            .start_time
            .1
            .checked_add(self.start_time.0.elapsed())
            .unwrap();

        match &self.crop {
            Some(cropped_area) => {
                // get the cropped area
                let start_x = cropped_area.origin.x as u32;
                let start_y = cropped_area.origin.y as u32;
                let end_x = (cropped_area.origin.x + cropped_area.size.width) as u32;
                let end_y = (cropped_area.origin.y + cropped_area.size.height) as u32;

                // crop the frame
                let cropped_buffer = frame
                    .buffer_crop(start_x, start_y, end_x, end_y)
                    .expect("Failed to crop buffer");

                // get raw frame buffer, avoiding a second full-frame copy when
                // the mapped rows carry padding: `as_nopadding_buffer` depads
                // exactly `w*h*4` bytes into the scratch Vec, so move that
                // Vec into the frame instead of `to_vec()`-ing it again
                // (one transient full-frame alloc+memcpy saved per delivered
                // frame). Without padding it borrows the mapped texture, so
                // that case still copies out with `to_vec()`.
                let mut nopadding_buf = Vec::new();
                let frame_data = if cropped_buffer.has_padding() {
                    // The return value just reborrows the scratch Vec, so
                    // discard it and move the Vec into the frame.
                    let _ = cropped_buffer.as_nopadding_buffer(&mut nopadding_buf);
                    debug_assert_eq!(
                        nopadding_buf.len(),
                        (end_x - start_x) as usize * (end_y - start_y) as usize * 4
                    );
                    nopadding_buf
                } else {
                    cropped_buffer
                        .as_nopadding_buffer(&mut nopadding_buf)
                        .to_vec()
                };

                let _current_time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Failed to get current time")
                    .as_nanos() as u64;

                let bgr_frame = BGRAFrame {
                    display_time,
                    width: cropped_area.size.width as i32,
                    height: cropped_area.size.height as i32,
                    data: frame_data,
                };

                // Bounded queue: drop the newest frame when a slow consumer
                // is behind (prevents unbounded memory growth / OOM).
                let _ = self.tx.try_send(Frame::Video(VideoFrame::BGRA(bgr_frame)));
            }
            None => {
                // NOTE: `FrameBuffer` borrows `frame`, so cache dimensions first.
                let width = frame.width() as i32;
                let height = frame.height() as i32;
                // get raw frame buffer
                let mut frame_buffer = frame.buffer().unwrap();
                let raw_frame_buffer = frame_buffer.as_raw_buffer();
                let frame_data = raw_frame_buffer.to_vec();
                let _current_time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("Failed to get current time")
                    .as_nanos() as u64;
                let bgr_frame = BGRAFrame {
                    display_time,
                    width,
                    height,
                    data: frame_data,
                };

                // Bounded queue: drop the newest frame when a slow consumer
                // is behind (prevents unbounded memory growth / OOM).
                let _ = self.tx.try_send(Frame::Video(VideoFrame::BGRA(bgr_frame)));
            }
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        println!("Closed");
        Ok(())
    }
}

impl WCStream {
    pub fn start_capture(&mut self) {
        let cc = match &self.settings {
            Settings::Display(st) => Capturer::start_free_threaded(st.to_owned()).unwrap(),
            Settings::Window(st) => Capturer::start_free_threaded(st.to_owned()).unwrap(),
        };

        if let Some(audio_stream) = &self.audio_stream {
            let _ = audio_stream.ctrl_tx.send(AudioStreamControl::Start);
        }

        self.capture_control = Some(cc)
    }

    pub fn stop_capture(&mut self) {
        let capture_control = self.capture_control.take().unwrap();
        let _ = capture_control.stop();

        if let Some(audio_stream) = &self.audio_stream {
            let _ = audio_stream.ctrl_tx.send(AudioStreamControl::Stop);
        }
    }
}

#[derive(Clone, Debug)]
struct FlagStruct {
    pub tx: FrameSender,
    pub crop: Option<Area>,
    /// Producer-side pacing, from `Options.fps` (see `Capturer::min_interval`).
    pub min_interval: Duration,
}

#[derive(Debug)]
pub enum CreateCapturerError {
    AudioStreamConfig(cpal::Error),
    BuildAudioStream(cpal::Error),
}

/// Map `Options.fps` to the OS-side Graphics Capture throttle.
/// fps >= 1 requests a custom minimum interval; fps == 0 leaves the default.
fn minimum_update_interval(fps: u32) -> MinimumUpdateIntervalSettings {
    if fps >= 1 {
        MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f32(1.0 / fps as f32))
    } else {
        MinimumUpdateIntervalSettings::Default
    }
}

pub fn create_capturer(
    options: &Options,
    tx: FrameSender,
) -> Result<WCStream, CreateCapturerError> {
    let target = options
        .target
        .clone()
        .unwrap_or_else(|| Target::Display(targets::get_main_display()));

    let color_format = match options.output_type {
        FrameType::BGRAFrame => ColorFormat::Bgra8,
        _ => ColorFormat::Rgba8,
    };

    let show_cursor = match options.show_cursor {
        true => CursorCaptureSettings::WithCursor,
        false => CursorCaptureSettings::WithoutCursor,
    };

    let draw_border = if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
        options
            .show_highlight
            .then_some(DrawBorderSettings::WithBorder)
            .unwrap_or(DrawBorderSettings::WithoutBorder)
    } else {
        DrawBorderSettings::Default
    };

    let minimum_update_interval = minimum_update_interval(options.fps);
    // The OS-side hint above is best-effort (Windows delivers on content
    // change regardless), so the handler enforces the same rate itself —
    // see `Capturer::min_interval`. `fps == 0` means unthrottled, preserving
    // the old behaviour for callers that drain as fast as they can.
    let min_interval = if options.fps >= 1 {
        Duration::from_secs_f32(1.0 / options.fps as f32)
    } else {
        Duration::ZERO
    };

    let settings = match target {
        Target::Display(display) => Settings::Display(WCSettings::new(
            WCMonitor::from_raw_hmonitor(display.raw_handle as *mut _),
            show_cursor,
            draw_border,
            SecondaryWindowSettings::Default,
            minimum_update_interval,
            DirtyRegionSettings::Default,
            color_format,
            FlagStruct {
                tx: tx.clone(),
                crop: Some(get_crop_area(options)),
                min_interval,
            },
        )),
        Target::Window(window) => Settings::Window(WCSettings::new(
            WCWindow::from_raw_hwnd(window.raw_handle as *mut _),
            show_cursor,
            draw_border,
            SecondaryWindowSettings::Default,
            minimum_update_interval,
            DirtyRegionSettings::Default,
            color_format,
            FlagStruct {
                tx: tx.clone(),
                crop: Some(get_crop_area(options)),
                min_interval,
            },
        )),
    };

    let audio_stream = if options.captures_audio {
        let (ctrl_tx, ctrl_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        spawn_audio_stream(tx.clone(), ready_tx, ctrl_rx);

        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => panic!("Audio spawn panicked"),
        }

        Some(AudioStreamHandle { ctrl_tx })
    } else {
        None
    };

    Ok(WCStream {
        settings,
        capture_control: None,
        audio_stream,
    })
}

pub fn get_output_frame_size(options: &Options) -> [u32; 2] {
    let _target = options
        .target
        .clone()
        .unwrap_or_else(|| Target::Display(targets::get_main_display()));

    let crop_area = get_crop_area(options);

    let mut output_width = (crop_area.size.width) as u32;
    let mut output_height = (crop_area.size.height) as u32;

    match options.output_resolution {
        Resolution::Captured => {}
        _ => {
            let [resolved_width, resolved_height] = options
                .output_resolution
                .value((crop_area.size.width as f32) / (crop_area.size.height as f32));
            // 1280 x 853
            output_width = cmp::min(output_width, resolved_width);
            output_height = cmp::min(output_height, resolved_height);
        }
    }

    output_width -= output_width % 2;
    output_height -= output_height % 2;

    [output_width, output_height]
}

fn get_absolute_value(value: f64, _scale_factor: f64) -> f64 {
    let value = (value).floor();
    value + value % 2.0
}

pub fn get_crop_area(options: &Options) -> Area {
    let target = options
        .target
        .clone()
        .unwrap_or_else(|| Target::Display(targets::get_main_display()));

    let (width, height) = targets::get_target_dimensions(&target);

    let scale_factor = targets::get_scale_factor(&target);
    options
        .crop_area
        .as_ref()
        .map(|val| {
            // WINDOWS: limit values [input-width, input-height] = [146, 50]
            Area {
                origin: Point {
                    x: get_absolute_value(val.origin.x, scale_factor),
                    y: get_absolute_value(val.origin.y, scale_factor),
                },
                size: Size {
                    width: get_absolute_value(val.size.width, scale_factor),
                    height: get_absolute_value(val.size.height, scale_factor),
                },
            }
        })
        .unwrap_or_else(|| Area {
            origin: Point { x: 0.0, y: 0.0 },
            size: Size {
                width: width as f64,
                height: height as f64,
            },
        })
}

struct AudioStreamHandle {
    ctrl_tx: mpsc::Sender<AudioStreamControl>,
}

#[derive(Debug)]
enum AudioStreamControl {
    Start,
    Stop,
}

fn build_audio_stream(
    sample_tx: mpsc::Sender<Result<(Vec<u8>, cpal::InputCallbackInfo, SystemTime), cpal::Error>>,
) -> Result<(cpal::Stream, cpal::SupportedStreamConfig), CreateCapturerError> {
    let host = cpal::default_host();
    let output_device =
        host.default_output_device()
            .ok_or(CreateCapturerError::AudioStreamConfig(
                cpal::ErrorKind::DeviceNotAvailable.into(),
            ))?;
    let supported_config = output_device
        .default_output_config()
        .map_err(CreateCapturerError::AudioStreamConfig)?;
    let config = supported_config.clone().into();

    let stream = output_device
        .build_input_stream_raw(
            config,
            supported_config.sample_format(),
            {
                let sample_tx = sample_tx.clone();
                move |data, info: &cpal::InputCallbackInfo| {
                    sample_tx
                        .send(Ok((data.bytes().to_vec(), info.clone(), SystemTime::now())))
                        .unwrap();
                }
            },
            move |e| {
                let _ = sample_tx.send(Err(e));
            },
            None,
        )
        .map_err(CreateCapturerError::BuildAudioStream)?;

    Ok((stream, supported_config))
}

fn spawn_audio_stream(
    tx: FrameSender,
    ready_tx: Sender<Result<(), CreateCapturerError>>,
    ctrl_rx: Receiver<AudioStreamControl>,
) {
    std::thread::spawn(move || {
        let (sample_tx, sample_rx) = mpsc::channel();

        let res = build_audio_stream(sample_tx);

        let (stream, config) = match res {
            Ok(stream) => {
                let _ = ready_tx.send(Ok(()));
                stream
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };

        let Ok(ctrl) = ctrl_rx.recv() else {
            return;
        };

        match ctrl {
            AudioStreamControl::Start => {
                stream.play().unwrap();
            }
            AudioStreamControl::Stop => {
                return;
            }
        }

        let audio_format = AudioFormat::from(config.sample_format());

        loop {
            match ctrl_rx.try_recv() {
                Ok(AudioStreamControl::Stop) => return,
                Ok(_) | Err(mpsc::TryRecvError::Empty) => {}
                Err(_) => return,
            };

            let (data, _info, timestamp) = match sample_rx.recv_timeout(Duration::from_millis(100))
            {
                Ok(Ok(data)) => data,
                Err(RecvTimeoutError::Timeout) => {
                    continue;
                }
                _ => {
                    // let _ = tx.send(Err(e));
                    return;
                }
            };

            let sample_count =
                data.len() / (audio_format.sample_size() * config.channels() as usize);
            let frame = AudioFrame::new(
                audio_format,
                config.channels(),
                false,
                data,
                sample_count,
                config.sample_rate(),
                timestamp,
            );

            // Bounded queue: drop audio when a slow consumer is behind;
            // stop only when the receiver is gone.
            match tx.try_send(Frame::Audio(frame)) {
                Err(mpsc::TrySendError::Disconnected(_)) => return,
                _ => {}
            };
        }
    });
}
