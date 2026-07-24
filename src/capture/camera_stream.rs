use std::{
    io::{self, ErrorKind, Read},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};

use crate::config::{Config, PreviewBackend};

#[cfg(target_os = "freebsd")]
use super::freebsd_v4l2;
use super::{
    ffmpeg::{camera_stream_ffmpeg_args, read_stderr_async},
    rgb_frame::{V4l2PixelFormat, frame_len},
};

const MIN_V4L2_STALL_TIMEOUT: Duration = Duration::from_secs(3);
const V4L2_MISSED_FRAME_LIMIT: u32 = 5;
const V4L2_RECONNECT_DELAY: Duration = Duration::from_millis(250);

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CameraFrameStatus {
    NewFrame,
    NoFrame,
    Ended,
}

#[derive(Default)]
pub(super) struct LatestCameraFrame {
    frame: Option<Vec<u8>>,
    ended: bool,
    error: Option<String>,
    serial: u64,
}

pub(super) fn store_latest_frame(
    state: &Arc<Mutex<LatestCameraFrame>>,
    frame: Vec<u8>,
    frame_len: usize,
) -> Vec<u8> {
    let old_frame = if let Ok(mut state) = state.lock() {
        let old_frame = state.frame.replace(frame);
        state.serial = state.serial.wrapping_add(1);
        old_frame
    } else {
        None
    };
    old_frame.unwrap_or_else(|| vec![0_u8; frame_len])
}

pub(super) fn mark_camera_stream_ended(state: &Arc<Mutex<LatestCameraFrame>>) {
    if let Ok(mut state) = state.lock() {
        state.ended = true;
    }
}

fn mark_camera_stream_error(state: &Arc<Mutex<LatestCameraFrame>>, error: io::Error) {
    if let Ok(mut state) = state.lock() {
        state.error = Some(error.to_string());
        state.ended = true;
    }
}

pub(crate) enum CameraStream {
    Ffmpeg(FfmpegCameraStream),
    V4l2(V4l2CameraStream),
}

impl CameraStream {
    pub(crate) fn spawn(config: &mut Config) -> Result<Self> {
        match config.preview_backend {
            PreviewBackend::Ffmpeg => FfmpegCameraStream::spawn(config).map(Self::Ffmpeg),
            PreviewBackend::V4l2 => V4l2CameraStream::spawn(config).map(Self::V4l2),
            PreviewBackend::Auto => V4l2CameraStream::spawn(config)
                .map(Self::V4l2)
                .or_else(|_| FfmpegCameraStream::spawn(config).map(Self::Ffmpeg)),
        }
    }

    pub(crate) fn read_latest_frame(
        &mut self,
        frame: &mut Vec<u8>,
    ) -> io::Result<CameraFrameStatus> {
        match self {
            Self::Ffmpeg(stream) => stream.read_latest_frame(frame),
            Self::V4l2(stream) => stream.read_latest_frame(frame),
        }
    }

    pub(crate) fn stderr_text(&self) -> String {
        match self {
            Self::Ffmpeg(stream) => stream.stderr_text(),
            Self::V4l2(stream) => stream.stderr_text(),
        }
    }

    pub(crate) fn stop(&mut self) {
        match self {
            Self::Ffmpeg(stream) => stream.stop(),
            Self::V4l2(stream) => stream.stop(),
        }
    }
}

pub(crate) struct FfmpegCameraStream {
    child: Child,
    latest_frame: Arc<Mutex<LatestCameraFrame>>,
    delivered_serial: u64,
    frame_thread: Option<thread::JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
}

impl FfmpegCameraStream {
    fn spawn(config: &Config) -> Result<Self> {
        let args = camera_stream_ffmpeg_args(config);

        let mut child = Command::new("ffmpeg")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start ffmpeg; is it installed and in PATH?")?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture ffmpeg stdout"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture ffmpeg stderr"))?;
        let (stderr, stderr_thread) = read_stderr_async(stderr_pipe);
        let frame_len = frame_len(config.width, config.height)?;
        let latest_frame = Arc::new(Mutex::new(LatestCameraFrame::default()));
        let frame_target = Arc::clone(&latest_frame);
        let frame_thread = thread::spawn(move || {
            let mut buffer = vec![0_u8; frame_len];
            loop {
                match stdout.read_exact(&mut buffer) {
                    Ok(()) => {
                        buffer = store_latest_frame(&frame_target, buffer, frame_len);
                    }
                    Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                        mark_camera_stream_ended(&frame_target);
                        break;
                    }
                    Err(error) => {
                        mark_camera_stream_error(&frame_target, error);
                        break;
                    }
                }
            }
        });

        Ok(Self {
            child,
            latest_frame,
            delivered_serial: 0,
            frame_thread: Some(frame_thread),
            stderr,
            stderr_thread: Some(stderr_thread),
        })
    }

    fn read_latest_frame(&mut self, frame: &mut Vec<u8>) -> io::Result<CameraFrameStatus> {
        read_latest_stored_frame(&self.latest_frame, &mut self.delivered_serial, frame)
    }

    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.frame_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for FfmpegCameraStream {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) struct V4l2CameraStream {
    latest_frame: Arc<Mutex<LatestCameraFrame>>,
    delivered_serial: u64,
    frame_thread: Option<thread::JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
}

impl V4l2CameraStream {
    fn spawn(config: &mut Config) -> Result<Self> {
        let latest_frame = Arc::new(Mutex::new(LatestCameraFrame::default()));
        let stderr = Arc::new(Mutex::new(String::new()));
        let frame_target = Arc::clone(&latest_frame);
        let stderr_target = Arc::clone(&stderr);
        let requested_config = config.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let frame_thread = thread::spawn(move || {
            run_v4l2_capture(&requested_config, &frame_target, &stderr_target, ready_tx);
        });

        match ready_rx
            .recv()
            .unwrap_or_else(|_| Err("v4l2 capture thread stopped during setup".to_string()))
        {
            Ok(info) => {
                config.width = info.width;
                config.height = info.height;
                config.input_format = Some(info.input_format.to_string());
                Ok(Self {
                    latest_frame,
                    delivered_serial: 0,
                    frame_thread: Some(frame_thread),
                    stderr,
                })
            }
            Err(error) => {
                let _ = frame_thread.join();
                Err(anyhow!(error))
            }
        }
    }

    fn read_latest_frame(&mut self, frame: &mut Vec<u8>) -> io::Result<CameraFrameStatus> {
        read_latest_stored_frame(&self.latest_frame, &mut self.delivered_serial, frame)
    }

    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    fn stop(&mut self) {
        mark_camera_stream_ended(&self.latest_frame);
        if let Some(handle) = self.frame_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for V4l2CameraStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn read_latest_stored_frame(
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    delivered_serial: &mut u64,
    frame: &mut Vec<u8>,
) -> io::Result<CameraFrameStatus> {
    let mut state = latest_frame
        .lock()
        .map_err(|_| io::Error::other("camera frame state is poisoned"))?;
    if state.serial != *delivered_serial {
        let Some(latest_frame) = state.frame.as_mut() else {
            return Ok(CameraFrameStatus::NoFrame);
        };
        if latest_frame.len() != frame.len() {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "camera frame has {} bytes, expected {}",
                    latest_frame.len(),
                    frame.len()
                ),
            ));
        }
        std::mem::swap(frame, latest_frame);
        *delivered_serial = state.serial;
        Ok(CameraFrameStatus::NewFrame)
    } else if let Some(error) = state.error.take() {
        Err(io::Error::other(error))
    } else if state.ended {
        Ok(CameraFrameStatus::Ended)
    } else {
        Ok(CameraFrameStatus::NoFrame)
    }
}

#[derive(Clone, Copy)]
pub(super) struct V4l2StreamInfo {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) input_format: &'static str,
}

pub(super) struct V4l2CaptureStartup<'a> {
    ready: &'a std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
    initial_info: Option<V4l2StreamInfo>,
}

impl<'a> V4l2CaptureStartup<'a> {
    fn new(
        ready: &'a std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
    ) -> Self {
        Self {
            ready,
            initial_info: None,
        }
    }

    pub(super) fn validate_info(&self, info: V4l2StreamInfo) -> std::result::Result<(), String> {
        if let Some(initial_info) = self.initial_info
            && (info.width != initial_info.width || info.height != initial_info.height)
        {
            return Err(format!(
                "recovered v4l2 stream changed dimensions from {}x{} to {}x{}",
                initial_info.width, initial_info.height, info.width, info.height
            ));
        }
        Ok(())
    }

    pub(super) fn report_ready(&mut self, info: V4l2StreamInfo) -> std::result::Result<(), String> {
        self.validate_info(info)?;
        if self.initial_info.is_some() {
            return Ok(());
        }
        self.ready
            .send(Ok(info))
            .map_err(|_| "camera startup receiver closed".to_string())?;
        self.initial_info = Some(info);
        Ok(())
    }

    fn started(&self) -> bool {
        self.initial_info.is_some()
    }
}

pub(super) struct V4l2FrameWatchdog {
    last_frame_at: Instant,
    timeout: Duration,
}

impl V4l2FrameWatchdog {
    pub(super) fn new(fps: u32) -> Self {
        Self::new_at(fps, Instant::now())
    }

    fn new_at(fps: u32, now: Instant) -> Self {
        let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)));
        Self {
            last_frame_at: now,
            timeout: MIN_V4L2_STALL_TIMEOUT
                .max(frame_interval.saturating_mul(V4L2_MISSED_FRAME_LIMIT)),
        }
    }

    pub(super) fn frame_received(&mut self) {
        self.frame_received_at(Instant::now());
    }

    fn frame_received_at(&mut self, now: Instant) {
        self.last_frame_at = now;
    }

    pub(super) fn stalled(&self) -> bool {
        self.stalled_at(Instant::now())
    }

    fn stalled_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_frame_at) >= self.timeout
    }

    pub(super) fn error(&self, error: &io::Error) -> String {
        format!(
            "v4l2 stream produced no frames for {:.1}s: {error}",
            self.timeout.as_secs_f64()
        )
    }
}

#[derive(Clone, Copy)]
pub(super) struct V4l2FrameConfig {
    pub(super) pixel_format: V4l2PixelFormat,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) mirror_horizontal: bool,
    pub(super) frame_len: usize,
}

pub(super) fn preferred_v4l2_formats(config: &Config) -> Vec<V4l2PixelFormat> {
    let mut formats = Vec::new();
    if let Some(input_format) = &config.input_format {
        let preferred = match input_format.as_str() {
            "mjpeg" | "MJPG" => Some(V4l2PixelFormat::Mjpeg),
            "yuyv422" | "YUYV" => Some(V4l2PixelFormat::Yuyv),
            "rgb24" | "RGB3" => Some(V4l2PixelFormat::Rgb24),
            _ => None,
        };
        if let Some(preferred) = preferred {
            formats.push(preferred);
        }
    }
    for format in [
        V4l2PixelFormat::Mjpeg,
        V4l2PixelFormat::Yuyv,
        V4l2PixelFormat::Rgb24,
    ] {
        if !formats.contains(&format) {
            formats.push(format);
        }
    }
    formats
}

fn run_v4l2_capture(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    stderr: &Arc<Mutex<String>>,
    ready: std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
) {
    let mut startup = V4l2CaptureStartup::new(&ready);
    loop {
        if startup.started() && camera_stream_should_stop(latest_frame) {
            return;
        }
        match run_v4l2_capture_inner(config, latest_frame, &mut startup) {
            Ok(()) => return,
            Err(error) if !startup.started() => {
                let _ = ready.send(Err(error.clone()));
                set_camera_stream_text(stderr, &error);
                mark_camera_stream_error(latest_frame, io::Error::other(error));
                return;
            }
            Err(error) => {
                set_camera_stream_text(
                    stderr,
                    &format!("camera stream interrupted: {error}; reconnecting"),
                );
                if camera_stream_should_stop(latest_frame) {
                    return;
                }
                thread::sleep(V4L2_RECONNECT_DELAY);
            }
        }
    }
}

fn run_v4l2_capture_inner(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    startup: &mut V4l2CaptureStartup<'_>,
) -> std::result::Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_v4l2_capture_platform(config, latest_frame, startup)
    }))
    .unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("unknown panic");
        Err(format!("v4l2 capture attempt panicked: {message}"))
    })
}

#[cfg(not(target_os = "freebsd"))]
fn run_v4l2_capture_platform(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    startup: &mut V4l2CaptureStartup<'_>,
) -> std::result::Result<(), String> {
    super::linux_v4l2::run_capture(config, latest_frame, startup)
}

#[cfg(target_os = "freebsd")]
fn run_v4l2_capture_platform(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    startup: &mut V4l2CaptureStartup<'_>,
) -> std::result::Result<(), String> {
    freebsd_v4l2::run_capture(config, latest_frame, startup)
}

pub(super) fn camera_stream_should_stop(state: &Arc<Mutex<LatestCameraFrame>>) -> bool {
    state.lock().map(|state| state.ended).unwrap_or(true)
}

pub(super) fn set_camera_stream_text(state: &Arc<Mutex<String>>, text: &str) {
    if let Ok(mut state) = state.lock() {
        *state = text.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_frame_store_keeps_only_newest_frame() {
        let state = Arc::new(Mutex::new(LatestCameraFrame::default()));
        let reused = store_latest_frame(&state, vec![1, 1, 1], 3);
        assert_eq!(reused, vec![0, 0, 0]);

        let reused = store_latest_frame(&state, vec![2, 2, 2], 3);
        assert_eq!(reused, vec![1, 1, 1]);

        let state = state.lock().expect("state should lock");
        assert_eq!(state.frame, Some(vec![2, 2, 2]));
        assert_eq!(state.serial, 2);
    }

    #[test]
    fn latest_frame_read_swaps_buffers_without_copying() {
        let state = Arc::new(Mutex::new(LatestCameraFrame::default()));
        let mut delivered_serial = 0;
        let mut app_frame = vec![1, 1, 1];

        let stored = store_latest_frame(&state, vec![2, 2, 2], 3);
        assert_eq!(stored, vec![0, 0, 0]);

        let status = read_latest_stored_frame(&state, &mut delivered_serial, &mut app_frame)
            .expect("latest frame should read");

        assert_eq!(status, CameraFrameStatus::NewFrame);
        assert_eq!(app_frame, vec![2, 2, 2]);
        let state = state.lock().expect("state should lock");
        assert_eq!(state.frame, Some(vec![1, 1, 1]));
    }

    #[test]
    fn v4l2_watchdog_uses_frame_rate_aware_stall_window() {
        let now = Instant::now();
        let fast = V4l2FrameWatchdog::new_at(30, now);
        assert!(!fast.stalled_at(now + Duration::from_millis(2_999)));
        assert!(fast.stalled_at(now + Duration::from_secs(3)));

        let slow = V4l2FrameWatchdog::new_at(1, now);
        assert!(!slow.stalled_at(now + Duration::from_millis(4_999)));
        assert!(slow.stalled_at(now + Duration::from_secs(5)));
    }

    #[test]
    fn v4l2_watchdog_resets_after_a_recovered_frame() {
        let now = Instant::now();
        let mut watchdog = V4l2FrameWatchdog::new_at(30, now);
        watchdog.frame_received_at(now + Duration::from_secs(2));

        assert!(!watchdog.stalled_at(now + Duration::from_secs(4)));
        assert!(watchdog.stalled_at(now + Duration::from_secs(5)));
    }

    #[test]
    fn v4l2_recovery_preserves_initial_frame_dimensions() {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let mut startup = V4l2CaptureStartup::new(&ready_tx);
        let initial = V4l2StreamInfo {
            width: 1920,
            height: 1080,
            input_format: "mjpeg",
        };
        startup.report_ready(initial).unwrap();

        let reported = ready_rx.recv().unwrap().unwrap();
        assert_eq!((reported.width, reported.height), (1920, 1080));
        startup.report_ready(initial).unwrap();
        assert!(ready_rx.try_recv().is_err());

        let error = startup
            .report_ready(V4l2StreamInfo {
                width: 1280,
                height: 720,
                input_format: "mjpeg",
            })
            .expect_err("a recovered stream must preserve RGB buffer dimensions");
        assert!(error.contains("changed dimensions from 1920x1080 to 1280x720"));
    }
}
