pub mod engine;

use std::{error::Error, sync::mpsc};

use engine::ChannelItem;

use crate::{
    frame::{Frame, FrameType},
    is_supported,
    targets::Target,
};

pub use engine::get_output_frame_size;

/// Bound on queued frames between the OS capture thread and the consumer.
///
/// All engines push into this queue; a slow consumer (e.g. 1 screenshot/sec
/// against a 60fps producer) used to grow it without limit -> OOM. With a
/// bounded queue the producers use non-blocking `try_send` and drop the
/// newest frame when full, so memory stays at `CAP x frame size` worst case
/// (1080p BGRA ~8.3MB -> ~33MB) on every OS. A keeping-up consumer never
/// sees a drop.
pub(crate) const FRAME_QUEUE_CAP: usize = 4;

#[derive(Debug, Clone, Copy, Default)]
pub enum Resolution {
    _480p,
    _720p,
    _1080p,
    _1440p,
    _2160p,
    _4320p,

    #[default]
    Captured,
}

impl Resolution {
    fn value(&self, aspect_ratio: f32) -> [u32; 2] {
        match *self {
            Resolution::_480p => [640, (640_f32 / aspect_ratio).floor() as u32],
            Resolution::_720p => [1280, (1280_f32 / aspect_ratio).floor() as u32],
            Resolution::_1080p => [1920, (1920_f32 / aspect_ratio).floor() as u32],
            Resolution::_1440p => [2560, (2560_f32 / aspect_ratio).floor() as u32],
            Resolution::_2160p => [3840, (3840_f32 / aspect_ratio).floor() as u32],
            Resolution::_4320p => [7680, (7680_f32 / aspect_ratio).floor() as u32],
            Resolution::Captured => {
                panic!(".value should not be called when Resolution type is Captured")
            }
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Default, Clone)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}
#[derive(Debug, Default, Clone)]
pub struct Area {
    pub origin: Point,
    pub size: Size,
}

/// Options passed to the screen capturer
#[derive(Debug, Default, Clone)]
pub struct Options {
    pub fps: u32,
    pub show_cursor: bool,
    pub show_highlight: bool,
    pub target: Option<Target>,
    pub crop_area: Option<Area>,
    pub output_type: FrameType,
    pub output_resolution: Resolution,
    // excluded targets will only work on macOS
    pub excluded_targets: Option<Vec<Target>>,
    /// Only implemented for Windows and macOS currently
    pub captures_audio: bool,
    pub exclude_current_process_audio: bool,
}

/// Screen capturer class
pub struct Capturer {
    engine: engine::Engine,
    rx: mpsc::Receiver<ChannelItem>,
}

/// Sender half of the frame queue. Bounded (see `FRAME_QUEUE_CAP`): producers
/// must use non-blocking `try_send` and drop on `Full` so a slow consumer
/// can never OOM the process, on any OS.
pub(crate) type FrameSender = mpsc::SyncSender<ChannelItem>;

#[derive(Debug)]
pub enum CapturerBuildError {
    NotSupported,
    PermissionNotGranted,
    Engine(String),
}

impl std::fmt::Display for CapturerBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapturerBuildError::NotSupported => write!(f, "Screen capturing is not supported"),
            CapturerBuildError::PermissionNotGranted => {
                write!(f, "Permission to capture the screen is not granted")
            }
            CapturerBuildError::Engine(error) => {
                write!(f, "Capture engine error: {error}")
            }
        }
    }
}

impl Error for CapturerBuildError {}

impl Capturer {
    /// Build a new [Capturer] instance with the provided options.
    ///
    /// Note: this deliberately does not check screen-recording permission first.
    /// On macOS 15+, attempting the capture is what summons the system's inline
    /// allow prompt ("bypass the privacy picker" authorisation); failing fast on
    /// a preflight check would prevent that prompt from ever appearing. A denial
    /// surfaces as an error from engine creation or `start_capture` instead.
    pub fn build(options: Options) -> Result<Capturer, CapturerBuildError> {
        if !is_supported() {
            return Err(CapturerBuildError::NotSupported);
        }

        let (tx, rx) = mpsc::sync_channel(FRAME_QUEUE_CAP);
        let engine = engine::Engine::new(&options, tx)?;

        Ok(Capturer { engine, rx })
    }

    // TODO
    // Prevent starting capture if already started
    /// Start capturing the frames.
    ///
    /// Returns an error if the operating system refuses the capture (e.g. the
    /// user denied the permission prompt) instead of panicking.
    pub fn start_capture(&mut self) -> Result<(), CapturerBuildError> {
        self.engine.start()
    }

    /// Stop the capturer
    pub fn stop_capture(&mut self) {
        self.engine.stop();
    }

    /// Get the next captured frame
    pub fn get_next_frame(&self) -> Result<Frame, mpsc::RecvError> {
        loop {
            let res = self.rx.recv()?;

            if let Some(frame) = self.engine.process_channel_item(res) {
                return Ok(frame);
            }
        }
    }

    /// Non-blocking drain step: returns the next queued video frame if one is
    /// ready, `Ok(None)` when the queue is empty, and `Err(RecvError)` when the
    /// producer has disconnected (same disconnect semantics as `get_next_frame`).
    ///
    /// Cross-platform: works on Windows, macOS and Linux.
    pub fn try_next_frame(&self) -> Result<Option<Frame>, mpsc::RecvError> {
        loop {
            match self.rx.try_recv() {
                Ok(item) => {
                    if let Some(frame) = self.engine.process_channel_item(item) {
                        return Ok(Some(frame));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(None),
                Err(mpsc::TryRecvError::Disconnected) => return Err(mpsc::RecvError),
            }
        }
    }

    /// Latest-frame grab for slow consumers (e.g. screenshot timers).
    ///
    /// Drains the whole queue and returns the newest video frame, dropping
    /// stale ones — memory stays flat no matter how slow you poll. Skips
    /// audio frames (call `try_next_frame` directly if you need audio).
    /// Returns `Ok(None)` when no video frame is queued.
    pub fn try_latest_video_frame(&self) -> Result<Option<Frame>, mpsc::RecvError> {
        let mut latest: Option<Frame> = None;
        loop {
            match self.try_next_frame()? {
                Some(frame @ Frame::Video(_)) => latest = Some(frame),
                Some(_) => continue, // skip audio, keep draining
                None => return Ok(latest),
            }
        }
    }

    /// Get the dimensions the frames will be captured in
    pub fn get_output_frame_size(&mut self) -> [u32; 2] {
        self.engine.get_output_frame_size()
    }

    pub fn raw(&self) -> RawCapturer<'_> {
        RawCapturer { capturer: self }
    }
}

pub struct RawCapturer<'a> {
    capturer: &'a Capturer,
}
