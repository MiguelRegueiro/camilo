use std::{
    io::{self, ErrorKind, Read},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

use anyhow::{Context, Result, anyhow};

use crate::config::{Config, PreviewBackend};

#[cfg(target_os = "freebsd")]
use super::freebsd_v4l2;
use super::{
    ffmpeg::{camera_stream_ffmpeg_args, read_stderr_async},
    rgb_frame::{V4l2PixelFormat, frame_len},
};

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
    if let Err(error) = run_v4l2_capture_inner(config, latest_frame, &ready) {
        let _ = ready.send(Err(error.clone()));
        set_camera_stream_text(stderr, &error);
        mark_camera_stream_error(latest_frame, io::Error::other(error));
    }
}

fn run_v4l2_capture_inner(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    ready: &std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
) -> std::result::Result<(), String> {
    run_v4l2_capture_platform(config, latest_frame, ready)
}

#[cfg(not(target_os = "freebsd"))]
fn run_v4l2_capture_platform(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    ready: &std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
) -> std::result::Result<(), String> {
    super::linux_v4l2::run_capture(config, latest_frame, ready)
}

#[cfg(target_os = "freebsd")]
fn run_v4l2_capture_platform(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    ready: &std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
) -> std::result::Result<(), String> {
    freebsd_v4l2::run_capture(config, latest_frame, ready)
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
}
