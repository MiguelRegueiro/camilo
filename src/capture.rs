use std::{
    ffi::OsStr,
    fs,
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use zune_jpeg::{JpegDecoder, zune_core::bytestream::ZCursor};

use crate::config::{Config, PreviewBackend};

pub(crate) const RAW_RGB_BYTES_PER_PIXEL: usize = 3;
pub(crate) const THUMBNAIL_SIZE: u32 = 160;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CameraMode {
    pub(crate) format: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
}

pub(crate) fn apply_best_camera_mode(config: &mut Config) {
    if let Ok(mode) = best_camera_mode(&config.device) {
        config.input_format = Some(ffmpeg_input_format(&mode.format).to_string());
        config.width = mode.width;
        config.height = mode.height;
    }
}

fn best_camera_mode(device: &str) -> Result<CameraMode> {
    best_v4l2_camera_mode(device)
}

#[cfg(not(target_os = "freebsd"))]
fn best_v4l2_camera_mode(device: &str) -> Result<CameraMode> {
    use v4l::prelude::*;
    use v4l::video::Capture;

    let device = Device::with_path(device).context("failed to open v4l2 device")?;
    let mut modes = Vec::new();

    for format in device.enum_formats()? {
        let Some(pixel_format) = V4l2PixelFormat::from_fourcc(v4l2_fourcc(format.fourcc.repr))
        else {
            continue;
        };

        for frame_size in device.enum_framesizes(format.fourcc)? {
            for discrete in frame_size.size.to_discrete() {
                let fps = best_v4l2_fps(&device, format.fourcc, discrete.width, discrete.height);
                modes.push(CameraMode {
                    format: pixel_format.fourcc_name().to_string(),
                    width: discrete.width,
                    height: discrete.height,
                    fps,
                });
            }
        }
    }

    modes
        .into_iter()
        .max_by_key(camera_mode_preference)
        .ok_or_else(|| anyhow!("camera did not report usable capture modes"))
}

#[cfg(target_os = "freebsd")]
fn best_v4l2_camera_mode(device: &str) -> Result<CameraMode> {
    freebsd_v4l2::best_camera_mode(device)
}

#[cfg(test)]
fn parse_best_camera_mode(text: &str) -> Option<CameraMode> {
    let mut format = None::<String>;
    let mut size = None::<(u32, u32)>;
    let mut modes = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            format = trimmed.split('\'').nth(1).map(str::to_string);
            size = None;
        } else if let Some(raw_size) = trimmed.strip_prefix("Size: Discrete ") {
            size = raw_size
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
        } else if let Some(interval) = trimmed.strip_prefix("Interval: Discrete ") {
            let fps = parse_interval_fps(interval)?;
            let (width, height) = size?;
            let format = format.clone()?;
            modes.push(CameraMode {
                format,
                width,
                height,
                fps,
            });
        }
    }

    modes.into_iter().max_by_key(camera_mode_preference)
}

fn camera_mode_preference(mode: &CameraMode) -> (u64, u32, u8) {
    (
        mode.width as u64 * mode.height as u64,
        mode.fps,
        u8::from(mode.format == "MJPG"),
    )
}

#[cfg(not(target_os = "freebsd"))]
fn best_v4l2_fps(device: &v4l::Device, fourcc: v4l::FourCC, width: u32, height: u32) -> u32 {
    use v4l::frameinterval::FrameIntervalEnum;
    use v4l::video::Capture;

    device
        .enum_frameintervals(fourcc, width, height)
        .ok()
        .and_then(|intervals| {
            intervals
                .into_iter()
                .filter_map(|interval| match interval.interval {
                    FrameIntervalEnum::Discrete(fraction) => {
                        fps_from_interval(fraction.numerator, fraction.denominator)
                    }
                    FrameIntervalEnum::Stepwise(stepwise) => {
                        fps_from_interval(stepwise.min.numerator, stepwise.min.denominator)
                    }
                })
                .max()
        })
        .unwrap_or(0)
}

#[cfg(test)]
fn parse_interval_fps(interval: &str) -> Option<u32> {
    let fps = interval
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(" fps)"))
        .and_then(|(value, _)| value.parse::<f64>().ok())
        .or_else(|| {
            let (seconds, _) = interval.split_once('s')?;
            let seconds = seconds.parse::<f64>().ok()?;
            (seconds > 0.0).then_some(1.0 / seconds)
        })?;
    Some(fps.round().clamp(1.0, 120.0) as u32)
}

fn ffmpeg_input_format(format: &str) -> &str {
    match format {
        "MJPG" => "mjpeg",
        "YUYV" => "yuyv422",
        "RGB3" => "rgb24",
        other => other,
    }
}

fn fps_from_interval(numerator: u32, denominator: u32) -> Option<u32> {
    (numerator > 0 && denominator > 0).then(|| {
        (f64::from(denominator) / f64::from(numerator))
            .round()
            .clamp(1.0, 120.0) as u32
    })
}

fn add_input_format_args(args: &mut Vec<String>, config: &Config) {
    if let Some(input_format) = &config.input_format {
        args.push("-input_format".to_string());
        args.push(input_format.clone());
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CaptureThumbnail {
    pub(crate) path: PathBuf,
    pub(crate) frame: Vec<u8>,
}

pub(crate) fn frame_len(width: u32, height: u32) -> Result<usize> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| anyhow!("frame dimensions are too large"))?;
    pixels
        .checked_mul(RAW_RGB_BYTES_PER_PIXEL as u32)
        .map(|v| v as usize)
        .ok_or_else(|| anyhow!("frame buffer is too large"))
}

pub(crate) fn save_capture(config: &Config, frame: &[u8]) -> Result<PathBuf> {
    fs::create_dir_all(&config.camera_dir).with_context(|| {
        format!(
            "failed to create camera directory {}",
            config.camera_dir.display()
        )
    })?;
    let path = capture_path(&config.camera_dir)?;
    let size = format!("{}x{}", config.width, config.height);

    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgb24",
            "-video_size",
            &size,
            "-i",
            "pipe:0",
            "-frames:v",
            "1",
            "-q:v",
            "2",
            "-y",
        ])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start ffmpeg to save capture")?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open ffmpeg capture input"))?;
    stdin
        .write_all(frame)
        .context("failed to send capture frame to ffmpeg")?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .context("failed to finish capture encoding")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("failed to save capture: {}", stderr.trim());
    }

    Ok(path)
}

fn recording_ffmpeg_args(
    config: &Config,
    path: &Path,
    video_size: &str,
    framerate: &str,
    audio: RecordingAudio,
) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-nostdin".to_string(),
        "-f".to_string(),
        "rawvideo".to_string(),
        "-pixel_format".to_string(),
        "rgb24".to_string(),
        "-video_size".to_string(),
        video_size.to_string(),
        "-framerate".to_string(),
        framerate.to_string(),
        "-i".to_string(),
        "pipe:0".to_string(),
    ];

    match audio {
        RecordingAudio::Input => {
            let (backend, input) = ffmpeg_audio_input(&config.audio_input);
            args.extend([
                "-f".to_string(),
                backend.to_string(),
                "-i".to_string(),
                input.to_string(),
            ]);
        }
        RecordingAudio::Silent => {
            args.extend([
                "-f".to_string(),
                "lavfi".to_string(),
                "-i".to_string(),
                "anullsrc=channel_layout=stereo:sample_rate=48000".to_string(),
            ]);
        }
        RecordingAudio::Disabled => {
            args.push("-an".to_string());
        }
    }

    args.extend([
        "-c:v".to_string(),
        "libx264".to_string(),
        "-preset".to_string(),
        "veryfast".to_string(),
        "-crf".to_string(),
        "18".to_string(),
        "-pix_fmt".to_string(),
        "yuv420p".to_string(),
    ]);

    if audio.encodes_track() {
        args.extend([
            "-c:a".to_string(),
            "aac".to_string(),
            "-b:a".to_string(),
            "128k".to_string(),
            "-shortest".to_string(),
        ]);
    }

    args.extend([
        "-movflags".to_string(),
        "+faststart".to_string(),
        "-y".to_string(),
        path.to_string_lossy().into_owned(),
    ]);

    args
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecordingAudio {
    Input,
    Silent,
    Disabled,
}

impl RecordingAudio {
    fn encodes_track(self) -> bool {
        matches!(self, Self::Input | Self::Silent)
    }
}

fn ffmpeg_audio_input(input: &str) -> (&str, &str) {
    input
        .split_once(':')
        .filter(|(backend, device)| is_supported_audio_backend(backend) && !device.is_empty())
        .unwrap_or(("pulse", input))
}

fn is_supported_audio_backend(backend: &str) -> bool {
    matches!(backend, "pulse" | "alsa" | "oss" | "avfoundation")
}

pub(crate) fn audio_input_available(config: &Config) -> bool {
    if !config.audio {
        return false;
    }
    let (backend, input) = ffmpeg_audio_input(&config.audio_input);
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            backend,
            "-i",
            input,
            "-t",
            "0.1",
            "-f",
            "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub(crate) struct VideoRecording {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<String>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
    path: PathBuf,
    audio: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RecordingWriteStatus {
    Written,
    RestartedWithoutAudio,
}

impl VideoRecording {
    pub(crate) fn start(config: &Config) -> Result<Self> {
        Self::start_with_audio(
            config,
            if config.audio {
                RecordingAudio::Input
            } else {
                RecordingAudio::Disabled
            },
        )
    }

    pub(crate) fn start_without_audio(config: &Config) -> Result<Self> {
        Self::start_with_audio(config, RecordingAudio::Silent)
    }

    fn start_with_audio(config: &Config, audio: RecordingAudio) -> Result<Self> {
        fs::create_dir_all(&config.camera_dir).with_context(|| {
            format!(
                "failed to create camera directory {}",
                config.camera_dir.display()
            )
        })?;
        let path = video_path(&config.camera_dir)?;
        let size = format!("{}x{}", config.width, config.height);
        let framerate = config.fps.to_string();

        let args = recording_ffmpeg_args(config, &path, &size, &framerate, audio);
        let mut child = Command::new("ffmpeg")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start ffmpeg to record video")?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to open ffmpeg video input"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture ffmpeg video stderr"))?;
        let (stderr, stderr_thread) = read_stderr_async(stderr_pipe);

        Ok(Self {
            child,
            stdin: Some(stdin),
            stderr,
            stderr_thread: Some(stderr_thread),
            path,
            audio: audio == RecordingAudio::Input,
        })
    }

    pub(crate) fn write_frame(
        &mut self,
        config: &Config,
        frame: &[u8],
    ) -> Result<RecordingWriteStatus> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("video recording input is closed"))?;
        match stdin.write_all(frame) {
            Ok(()) => Ok(RecordingWriteStatus::Written),
            Err(error) if self.audio && error.kind() == ErrorKind::BrokenPipe => {
                self.restart_without_audio(config, frame)
            }
            Err(error) => Err(error).context("failed to send frame to video encoder"),
        }
    }

    pub(crate) fn stop_async(self) -> thread::JoinHandle<Result<()>> {
        thread::spawn(move || self.stop())
    }

    pub(crate) fn stop(mut self) -> Result<()> {
        drop(self.stdin.take());
        let status = self
            .child
            .wait()
            .context("failed to finish video recording")?;
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
        if !status.success() {
            let stderr = self.stderr_text();
            bail!("failed to record video: {}", stderr.trim());
        }
        Ok(())
    }

    pub(crate) fn audio(&self) -> bool {
        self.audio
    }

    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    fn restart_without_audio(
        &mut self,
        config: &Config,
        frame: &[u8],
    ) -> Result<RecordingWriteStatus> {
        drop(self.stdin.take());
        let status = self
            .child
            .wait()
            .context("failed to inspect failed video recording")?;
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
        let stderr = self.stderr_text();
        if status.success() || !looks_like_audio_input_failure(&stderr) {
            bail!("failed to send frame to video encoder: {}", stderr.trim());
        }

        let failed_path = std::mem::take(&mut self.path);
        if failed_path.exists() {
            let _ = fs::remove_file(&failed_path);
        }

        let mut replacement = Self::start_with_audio(config, RecordingAudio::Silent)?;
        replacement
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("video recording input is closed"))?
            .write_all(frame)
            .context("failed to send frame to silent video encoder")?;
        *self = replacement;
        Ok(RecordingWriteStatus::RestartedWithoutAudio)
    }
}

fn looks_like_audio_input_failure(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("unknown input format")
        || stderr.contains("no such process")
        || stderr.contains("no such file or directory")
        || stderr.contains("audio input")
        || stderr.contains("cannot open audio")
        || stderr.contains("cannot open input")
        || stderr.contains("pulse")
        || stderr.contains("alsa")
        || stderr.contains("oss")
        || stderr.contains("avfoundation")
}

impl Drop for VideoRecording {
    fn drop(&mut self) {
        if self.stdin.is_some() {
            drop(self.stdin.take());
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(handle) = self.stderr_thread.take() {
                let _ = handle.join();
            }
        }
    }
}

fn capture_path(camera_dir: &Path) -> Result<PathBuf> {
    timestamped_media_path(camera_dir, "capture", "jpg")
}

fn video_path(camera_dir: &Path) -> Result<PathBuf> {
    timestamped_media_path(camera_dir, "video", "mp4")
}

fn timestamped_media_path(camera_dir: &Path, prefix: &str, extension: &str) -> Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    let stem = format!("{prefix}-{}-{:09}", now.as_secs(), now.subsec_nanos());
    Ok(camera_dir.join(format!("{stem}.{extension}")))
}

pub(crate) fn latest_capture_thumbnail(camera_dir: &Path, size: u32) -> Option<CaptureThumbnail> {
    let path = latest_image_path(camera_dir)?;
    let frame = load_image_thumbnail(&path, size).ok()?;
    Some(CaptureThumbnail { path, frame })
}

pub(crate) fn latest_image_path(camera_dir: &Path) -> Option<PathBuf> {
    fs::read_dir(camera_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| is_supported_capture_image(path))
        .filter_map(|path| {
            let modified = path
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()?;
            Some((modified, path))
        })
        .max_by_key(|(modified, path)| (*modified, path.clone()))
        .map(|(_, path)| path)
}

fn is_supported_capture_image(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png"
            )
        })
        .unwrap_or(false)
}

fn load_image_thumbnail(path: &Path, size: u32) -> Result<Vec<u8>> {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-i",
        ])
        .arg(path)
        .args([
            "-vf",
            &format!(
                "scale={size}:{size}:force_original_aspect_ratio=increase,crop={size}:{size}:(iw-ow)/2:(ih-oh)/2,format=rgb24"
            ),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("failed to load thumbnail from {}", path.display()))?;

    if !output.status.success() {
        bail!("ffmpeg could not decode {}", path.display());
    }

    let expected_len = frame_len(size, size)?;
    if output.stdout.len() != expected_len {
        bail!(
            "thumbnail for {} has {} bytes, expected {expected_len}",
            path.display(),
            output.stdout.len()
        );
    }

    Ok(output.stdout)
}

pub(crate) fn square_thumbnail(
    frame: &[u8],
    source_width: u32,
    source_height: u32,
    size: u32,
) -> Vec<u8> {
    let output_len = frame_len(size, size).unwrap_or(0);
    let mut out = vec![0_u8; output_len];
    if source_width == 0 || source_height == 0 || size == 0 {
        return out;
    }

    let crop_size = source_width.min(source_height).max(1);
    let crop_x = (source_width - crop_size) / 2;
    let crop_y = (source_height - crop_size) / 2;

    for y in 0..size {
        let src_y = crop_y + y.saturating_mul(crop_size) / size;
        for x in 0..size {
            let src_x = crop_x + x.saturating_mul(crop_size) / size;
            let src = ((src_y * source_width + src_x) * RAW_RGB_BYTES_PER_PIXEL as u32) as usize;
            let dst = ((y * size + x) * RAW_RGB_BYTES_PER_PIXEL as u32) as usize;
            if src + RAW_RGB_BYTES_PER_PIXEL <= frame.len()
                && dst + RAW_RGB_BYTES_PER_PIXEL <= out.len()
            {
                out[dst..dst + RAW_RGB_BYTES_PER_PIXEL]
                    .copy_from_slice(&frame[src..src + RAW_RGB_BYTES_PER_PIXEL]);
            }
        }
    }

    out
}

fn read_stderr_async(stderr_pipe: ChildStderr) -> (Arc<Mutex<String>>, thread::JoinHandle<()>) {
    let stderr = Arc::new(Mutex::new(String::new()));
    let stderr_target = Arc::clone(&stderr);
    let stderr_thread = thread::spawn(move || {
        let mut stderr_pipe = stderr_pipe;
        let mut text = String::new();
        let _ = stderr_pipe.read_to_string(&mut text);
        if let Ok(mut target) = stderr_target.lock() {
            *target = text;
        }
    });
    (stderr, stderr_thread)
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CameraFrameStatus {
    NewFrame,
    NoFrame,
    Ended,
}

#[derive(Default)]
struct LatestCameraFrame {
    frame: Option<Vec<u8>>,
    ended: bool,
    error: Option<String>,
    serial: u64,
}

fn store_latest_frame(
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

fn mark_camera_stream_ended(state: &Arc<Mutex<LatestCameraFrame>>) {
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

fn camera_stream_ffmpeg_args(config: &Config) -> Vec<String> {
    let video_filter = ffmpeg_video_filter(config);
    let input_size = format!("{}x{}", config.width, config.height);
    let framerate = config.fps.to_string();
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-nostdin".to_string(),
        "-fflags".to_string(),
        "nobuffer".to_string(),
        "-flags".to_string(),
        "low_delay".to_string(),
        "-f".to_string(),
        "v4l2".to_string(),
    ];
    add_input_format_args(&mut args, config);
    args.extend([
        "-thread_queue_size".to_string(),
        "1".to_string(),
        "-framerate".to_string(),
        framerate,
        "-video_size".to_string(),
        input_size,
        "-i".to_string(),
        config.device.clone(),
        "-an".to_string(),
        "-sn".to_string(),
        "-dn".to_string(),
        "-vf".to_string(),
        video_filter,
        "-pix_fmt".to_string(),
        "rgb24".to_string(),
        "-f".to_string(),
        "rawvideo".to_string(),
        "pipe:1".to_string(),
    ]);
    args
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

    pub(crate) fn read_latest_frame(&mut self, frame: &mut [u8]) -> io::Result<CameraFrameStatus> {
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

    fn read_latest_frame(&mut self, frame: &mut [u8]) -> io::Result<CameraFrameStatus> {
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

    fn read_latest_frame(&mut self, frame: &mut [u8]) -> io::Result<CameraFrameStatus> {
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
    frame: &mut [u8],
) -> io::Result<CameraFrameStatus> {
    let mut state = latest_frame
        .lock()
        .map_err(|_| io::Error::other("camera frame state is poisoned"))?;
    if state.serial != *delivered_serial {
        let Some(latest_frame) = state.frame.as_ref() else {
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
        frame.copy_from_slice(latest_frame);
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum V4l2PixelFormat {
    Rgb24,
    Yuyv,
    Mjpeg,
}

impl V4l2PixelFormat {
    fn fourcc(self) -> u32 {
        v4l2_fourcc(self.fourcc_bytes())
    }

    fn fourcc_bytes(self) -> [u8; 4] {
        match self {
            Self::Rgb24 => *b"RGB3",
            Self::Yuyv => *b"YUYV",
            Self::Mjpeg => *b"MJPG",
        }
    }

    fn from_fourcc(fourcc: u32) -> Option<Self> {
        if fourcc == Self::Rgb24.fourcc() {
            Some(Self::Rgb24)
        } else if fourcc == Self::Yuyv.fourcc() {
            Some(Self::Yuyv)
        } else if fourcc == Self::Mjpeg.fourcc() {
            Some(Self::Mjpeg)
        } else {
            None
        }
    }

    fn fourcc_name(self) -> &'static str {
        match self {
            Self::Rgb24 => "RGB3",
            Self::Yuyv => "YUYV",
            Self::Mjpeg => "MJPG",
        }
    }

    fn input_format(self) -> &'static str {
        match self {
            Self::Rgb24 => "rgb24",
            Self::Yuyv => "yuyv422",
            Self::Mjpeg => "mjpeg",
        }
    }
}

const fn v4l2_fourcc(bytes: [u8; 4]) -> u32 {
    bytes[0] as u32
        | ((bytes[1] as u32) << 8)
        | ((bytes[2] as u32) << 16)
        | ((bytes[3] as u32) << 24)
}

#[derive(Clone, Copy)]
struct V4l2StreamInfo {
    width: u32,
    height: u32,
    input_format: &'static str,
}

#[derive(Clone, Copy)]
struct V4l2FrameConfig {
    pixel_format: V4l2PixelFormat,
    width: u32,
    height: u32,
    mirror_horizontal: bool,
    frame_len: usize,
}

fn preferred_v4l2_formats(config: &Config) -> Vec<V4l2PixelFormat> {
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
    run_v4l2_capture_inner_platform(config, latest_frame, ready)
}

#[cfg(not(target_os = "freebsd"))]
fn run_v4l2_capture_inner_platform(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    ready: &std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
) -> std::result::Result<(), String> {
    use v4l::buffer::Type;
    use v4l::io::mmap::Stream as MmapStream;
    use v4l::prelude::*;
    use v4l::video::Capture;
    use v4l::video::capture::Parameters;

    let device = Device::with_path(&config.device)
        .map_err(|error| format!("failed to open v4l2 device {}: {error}", config.device))?;

    let mut setup_errors = Vec::new();
    let Some((mut stream, frame_config, mut buffer)) = preferred_v4l2_formats(config)
        .into_iter()
        .find_map(|pixel_format| {
            let requested = v4l::Format::new(
                config.width,
                config.height,
                v4l::FourCC::new(&pixel_format.fourcc_bytes()),
            );
            let actual = match device.set_format(&requested) {
                Ok(actual) => actual,
                Err(error) => {
                    setup_errors.push(format!("{pixel_format:?}: {error}"));
                    return None;
                }
            };
            let Some(actual_format) = V4l2PixelFormat::from_fourcc(v4l2_fourcc(actual.fourcc.repr))
            else {
                setup_errors.push(format!(
                    "{pixel_format:?}: device selected unsupported {}",
                    actual.fourcc
                ));
                return None;
            };
            if let Err(error) = device.set_params(&Parameters::with_fps(config.fps)) {
                setup_errors.push(format!(
                    "{actual_format:?}: failed to set fps {}: {error}",
                    config.fps
                ));
                return None;
            }
            let frame_len = match frame_len(actual.width, actual.height) {
                Ok(frame_len) => frame_len,
                Err(error) => {
                    setup_errors.push(format!(
                        "{actual_format:?}: invalid frame size {}x{}: {error}",
                        actual.width, actual.height
                    ));
                    return None;
                }
            };
            let frame_config = V4l2FrameConfig {
                pixel_format: actual_format,
                width: actual.width,
                height: actual.height,
                mirror_horizontal: config.mirror_horizontal,
                frame_len,
            };
            let mut stream = match MmapStream::with_buffers(&device, Type::VideoCapture, 2) {
                Ok(stream) => stream,
                Err(error) => {
                    setup_errors.push(format!(
                        "{actual_format:?}: failed to create mmap stream: {error}"
                    ));
                    return None;
                }
            };
            stream.set_timeout(std::time::Duration::from_secs(3));
            let mut buffer = vec![0_u8; frame_config.frame_len];
            match store_v4l2_next_frame(&mut stream, frame_config, latest_frame, &mut buffer) {
                Ok(reused) => {
                    stream.set_timeout(std::time::Duration::from_millis(100));
                    Some((stream, frame_config, reused))
                }
                Err(error) => {
                    setup_errors.push(format!(
                        "{actual_format:?}: failed to read first frame: {error}"
                    ));
                    None
                }
            }
        })
    else {
        return Err(format!(
            "v4l2 device did not produce a frame for a supported RGB/YUYV/MJPG preview mode near {}x{}{}",
            config.width,
            config.height,
            if setup_errors.is_empty() {
                String::new()
            } else {
                format!(": {}", setup_errors.join("; "))
            }
        ));
    };
    let _ = ready.send(Ok(V4l2StreamInfo {
        width: frame_config.width,
        height: frame_config.height,
        input_format: frame_config.pixel_format.input_format(),
    }));

    loop {
        if camera_stream_should_stop(latest_frame) {
            break;
        }
        match store_v4l2_next_frame(&mut stream, frame_config, latest_frame, &mut buffer) {
            Ok(reused) => buffer = reused,
            Err(error) if error.kind() == ErrorKind::TimedOut => {}
            Err(error) if is_transient_v4l2_runtime_read_error(&error) => {
                thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(error) => return Err(format!("failed to read v4l2 frame: {error}")),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn is_transient_v4l2_runtime_read_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(22)
}

#[cfg(target_os = "freebsd")]
fn run_v4l2_capture_inner_platform(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    ready: &std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
) -> std::result::Result<(), String> {
    freebsd_v4l2::run_capture(config, latest_frame, ready)
}

fn camera_stream_should_stop(state: &Arc<Mutex<LatestCameraFrame>>) -> bool {
    state.lock().map(|state| state.ended).unwrap_or(true)
}

#[cfg(not(target_os = "freebsd"))]
fn store_v4l2_next_frame(
    stream: &mut v4l::io::mmap::Stream<'_>,
    config: V4l2FrameConfig,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    buffer: &mut [u8],
) -> io::Result<Vec<u8>> {
    use v4l::io::traits::CaptureStream;

    let (frame, metadata) = stream.next()?;
    let used = (metadata.bytesused as usize).min(frame.len());
    decode_v4l2_frame(
        config.pixel_format,
        &frame[..used],
        config.width,
        config.height,
        buffer,
    )
    .map_err(io::Error::other)?;
    if config.mirror_horizontal {
        mirror_rgb24_in_place(buffer, config.width, config.height);
    }
    Ok(store_latest_frame(
        latest_frame,
        buffer.to_vec(),
        config.frame_len,
    ))
}

fn decode_v4l2_frame(
    pixel_format: V4l2PixelFormat,
    frame: &[u8],
    width: u32,
    height: u32,
    out: &mut [u8],
) -> std::result::Result<(), String> {
    match pixel_format {
        V4l2PixelFormat::Rgb24 => copy_rgb24_frame(frame, out),
        V4l2PixelFormat::Yuyv => convert_yuyv_to_rgb24(frame, width, height, out),
        V4l2PixelFormat::Mjpeg => decode_mjpeg_to_rgb24(frame, width, height, out),
    }
}

fn copy_rgb24_frame(frame: &[u8], out: &mut [u8]) -> std::result::Result<(), String> {
    if frame.len() < out.len() {
        return Err(format!(
            "rgb24 frame has {} bytes, expected at least {}",
            frame.len(),
            out.len()
        ));
    }
    out.copy_from_slice(&frame[..out.len()]);
    Ok(())
}

fn convert_yuyv_to_rgb24(
    frame: &[u8],
    width: u32,
    height: u32,
    out: &mut [u8],
) -> std::result::Result<(), String> {
    let expected_in = width as usize * height as usize * 2;
    let expected_out = width as usize * height as usize * RAW_RGB_BYTES_PER_PIXEL;
    if frame.len() < expected_in || out.len() < expected_out {
        return Err(format!(
            "yuyv frame has {} bytes and output has {}, expected {expected_in}/{expected_out}",
            frame.len(),
            out.len()
        ));
    }

    let mut dst = 0;
    for chunk in frame[..expected_in].chunks_exact(4) {
        let y0 = chunk[0];
        let u = chunk[1];
        let y1 = chunk[2];
        let v = chunk[3];
        let [r, g, b] = yuv_to_rgb(y0, u, v);
        out[dst..dst + 3].copy_from_slice(&[r, g, b]);
        let [r, g, b] = yuv_to_rgb(y1, u, v);
        out[dst + 3..dst + 6].copy_from_slice(&[r, g, b]);
        dst += 6;
    }
    Ok(())
}

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = i32::from(y).saturating_sub(16).max(0);
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    [
        clamp_rgb((298 * c + 409 * e + 128) >> 8),
        clamp_rgb((298 * c - 100 * d - 208 * e + 128) >> 8),
        clamp_rgb((298 * c + 516 * d + 128) >> 8),
    ]
}

fn clamp_rgb(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn decode_mjpeg_to_rgb24(
    frame: &[u8],
    width: u32,
    height: u32,
    out: &mut [u8],
) -> std::result::Result<(), String> {
    let mut decoder = JpegDecoder::new(ZCursor::new(frame));
    let decoded = decoder
        .decode()
        .map_err(|error| format!("failed to decode mjpeg frame: {error}"))?;
    let info = decoder
        .info()
        .ok_or_else(|| "mjpeg frame did not report dimensions".to_string())?;
    if usize::from(info.width) != width as usize || usize::from(info.height) != height as usize {
        return Err(format!(
            "mjpeg frame is {}x{}, expected {}x{}",
            info.width, info.height, width, height
        ));
    }
    if decoded.len() != out.len() {
        return Err(format!(
            "mjpeg decoded to {} bytes, expected {} rgb bytes",
            decoded.len(),
            out.len()
        ));
    }
    out.copy_from_slice(&decoded);
    Ok(())
}

fn mirror_rgb24_in_place(frame: &mut [u8], width: u32, height: u32) {
    let width = width as usize;
    let height = height as usize;
    let stride = width * RAW_RGB_BYTES_PER_PIXEL;
    for y in 0..height {
        let row = y * stride;
        for x in 0..width / 2 {
            let left = row + x * RAW_RGB_BYTES_PER_PIXEL;
            let right = row + (width - 1 - x) * RAW_RGB_BYTES_PER_PIXEL;
            for channel in 0..RAW_RGB_BYTES_PER_PIXEL {
                frame.swap(left + channel, right + channel);
            }
        }
    }
}

#[cfg(target_os = "freebsd")]
mod freebsd_v4l2 {
    use std::{
        ffi::CString,
        io::{self, ErrorKind},
        mem,
        os::raw::{c_char, c_int, c_ulong, c_void},
        ptr,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use anyhow::{Context, Result, anyhow};

    use super::{
        CameraMode, Config, LatestCameraFrame, V4l2FrameConfig, V4l2PixelFormat, V4l2StreamInfo,
        camera_mode_preference, camera_stream_should_stop, decode_v4l2_frame, frame_len,
        mark_camera_stream_ended, mirror_rgb24_in_place, store_latest_frame,
    };

    const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
    const V4L2_MEMORY_MMAP: u32 = 1;
    const V4L2_FRMSIZE_TYPE_DISCRETE: u32 = 1;
    const V4L2_FRMSIZE_TYPE_STEPWISE: u32 = 3;
    const V4L2_FRMIVAL_TYPE_DISCRETE: u32 = 1;
    const V4L2_FRMIVAL_TYPE_STEPWISE: u32 = 3;

    const VIDIOC_ENUM_FMT: c_ulong = 0xc040_5602;
    const VIDIOC_S_FMT: c_ulong = 0xc0d0_5605;
    const VIDIOC_REQBUFS: c_ulong = 0xc014_5608;
    const VIDIOC_QUERYBUF: c_ulong = 0xc058_5609;
    const VIDIOC_QBUF: c_ulong = 0xc058_560f;
    const VIDIOC_DQBUF: c_ulong = 0xc058_5611;
    const VIDIOC_STREAMON: c_ulong = 0x8004_5612;
    const VIDIOC_STREAMOFF: c_ulong = 0x8004_5613;
    const VIDIOC_S_PARM: c_ulong = 0xc0cc_5616;
    const VIDIOC_ENUM_FRAMESIZES: c_ulong = 0xc02c_564a;
    const VIDIOC_ENUM_FRAMEINTERVALS: c_ulong = 0xc034_564b;

    unsafe extern "C" {
        #[link_name = "v4l2_open"]
        fn libv4l2_open(file: *const c_char, oflag: c_int, ...) -> c_int;
        #[link_name = "v4l2_close"]
        fn libv4l2_close(fd: c_int) -> c_int;
        #[link_name = "v4l2_ioctl"]
        fn libv4l2_ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        #[link_name = "v4l2_mmap"]
        fn libv4l2_mmap(
            start: *mut c_void,
            length: usize,
            prot: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        #[link_name = "v4l2_munmap"]
        fn libv4l2_munmap(start: *mut c_void, length: usize) -> c_int;
    }

    #[link(name = "v4l2")]
    unsafe extern "C" {}

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct V4l2Fract {
        numerator: u32,
        denominator: u32,
    }

    #[repr(C)]
    struct V4l2Fmtdesc {
        index: u32,
        type_: u32,
        flags: u32,
        description: [u8; 32],
        pixelformat: u32,
        mbus_code: u32,
        reserved: [u32; 3],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct V4l2PixFormat {
        width: u32,
        height: u32,
        pixelformat: u32,
        field: u32,
        bytesperline: u32,
        sizeimage: u32,
        colorspace: u32,
        priv_: u32,
        flags: u32,
        ycbcr_enc: u32,
        quantization: u32,
        xfer_func: u32,
    }

    #[repr(C)]
    struct V4l2Format {
        type_: u32,
        union_align: u32,
        pix: V4l2PixFormat,
        padding: [u8; 152],
    }

    #[repr(C)]
    struct V4l2StreamParmCapture {
        capability: u32,
        capturemode: u32,
        timeperframe: V4l2Fract,
        extendedmode: u32,
        readbuffers: u32,
        reserved: [u32; 4],
    }

    #[repr(C)]
    struct V4l2StreamParm {
        type_: u32,
        capture: V4l2StreamParmCapture,
        padding: [u8; 160],
    }

    #[repr(C)]
    struct V4l2RequestBuffers {
        count: u32,
        type_: u32,
        memory: u32,
        capabilities: u32,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct V4l2Timecode {
        type_: u32,
        flags: u32,
        frames: u8,
        seconds: u8,
        minutes: u8,
        hours: u8,
        userbits: [u8; 4],
    }

    #[repr(C)]
    struct V4l2Buffer {
        index: u32,
        type_: u32,
        bytesused: u32,
        flags: u32,
        field: u32,
        timestamp: libc::timeval,
        timecode: V4l2Timecode,
        sequence: u32,
        memory: u32,
        m_offset: u32,
        m_padding: u32,
        length: u32,
        reserved2: u32,
        request_fd: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct V4l2FrmSizeDiscrete {
        width: u32,
        height: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct V4l2FrmSizeStepwise {
        min_width: u32,
        max_width: u32,
        step_width: u32,
        min_height: u32,
        max_height: u32,
        step_height: u32,
    }

    #[repr(C)]
    union V4l2FrmSizeUnion {
        discrete: V4l2FrmSizeDiscrete,
        stepwise: V4l2FrmSizeStepwise,
    }

    #[repr(C)]
    struct V4l2FrmSizeEnum {
        index: u32,
        pixel_format: u32,
        type_: u32,
        size: V4l2FrmSizeUnion,
        reserved: [u32; 2],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct V4l2FrmIvalStepwise {
        min: V4l2Fract,
        max: V4l2Fract,
        step: V4l2Fract,
    }

    #[repr(C)]
    union V4l2FrmIvalUnion {
        discrete: V4l2Fract,
        stepwise: V4l2FrmIvalStepwise,
    }

    #[repr(C)]
    struct V4l2FrmIvalEnum {
        index: u32,
        pixel_format: u32,
        width: u32,
        height: u32,
        type_: u32,
        interval: V4l2FrmIvalUnion,
        reserved: [u32; 2],
    }

    struct Device {
        fd: c_int,
    }

    impl Device {
        fn open(path: &str) -> io::Result<Self> {
            let path = CString::new(path).map_err(|_| {
                io::Error::new(ErrorKind::InvalidInput, "device path contains a NUL byte")
            })?;
            let fd = unsafe { libv4l2_open(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self { fd })
            }
        }

        fn ioctl<T>(&self, request: c_ulong, arg: &mut T) -> io::Result<()> {
            ioctl(self.fd, request, arg)
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            let _ = unsafe { libv4l2_close(self.fd) };
        }
    }

    struct MappedBuffer {
        ptr: *mut u8,
        len: usize,
    }

    struct MmapStream {
        fd: c_int,
        buffers: Vec<MappedBuffer>,
        streaming: bool,
    }

    impl MmapStream {
        fn new(device: &Device, requested_count: u32) -> io::Result<Self> {
            let mut req = V4l2RequestBuffers {
                count: requested_count,
                type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
                memory: V4L2_MEMORY_MMAP,
                capabilities: 0,
                flags: 0,
            };
            device.ioctl(VIDIOC_REQBUFS, &mut req)?;
            if req.count == 0 {
                return Err(io::Error::other("device did not allocate mmap buffers"));
            }

            let mut buffers = Vec::with_capacity(req.count as usize);
            for index in 0..req.count {
                let mut buf = v4l2_buffer(index);
                device.ioctl(VIDIOC_QUERYBUF, &mut buf)?;
                let mapped = unsafe {
                    libv4l2_mmap(
                        ptr::null_mut(),
                        buf.length as usize,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        device.fd,
                        i64::from(buf.m_offset),
                    )
                };
                if mapped == libc::MAP_FAILED {
                    return Err(io::Error::last_os_error());
                }
                buffers.push(MappedBuffer {
                    ptr: mapped.cast::<u8>(),
                    len: buf.length as usize,
                });
                device.ioctl(VIDIOC_QBUF, &mut buf)?;
            }

            let mut stream = Self {
                fd: device.fd,
                buffers,
                streaming: false,
            };
            let mut type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            device.ioctl(VIDIOC_STREAMON, &mut type_)?;
            stream.streaming = true;
            Ok(stream)
        }

        fn next_frame(&mut self, timeout: Duration) -> io::Result<(&[u8], usize)> {
            let mut pollfd = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout_ms = timeout.as_millis().min(c_int::MAX as u128) as c_int;
            let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
            if ready == 0 {
                return Err(io::Error::new(ErrorKind::TimedOut, "v4l2 frame timed out"));
            }
            if ready < 0 {
                return Err(io::Error::last_os_error());
            }

            let mut buf = v4l2_buffer(0);
            ioctl(self.fd, VIDIOC_DQBUF, &mut buf)?;
            let index = buf.index as usize;
            let Some(mapped) = self.buffers.get(index) else {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!("device dequeued invalid buffer index {}", buf.index),
                ));
            };
            let used = (buf.bytesused as usize).min(mapped.len);
            let frame = unsafe { std::slice::from_raw_parts(mapped.ptr, used) };
            Ok((frame, index))
        }

        fn queue_buffer(&self, index: usize) -> io::Result<()> {
            let mut buf = v4l2_buffer(index as u32);
            ioctl(self.fd, VIDIOC_QBUF, &mut buf)
        }
    }

    impl Drop for MmapStream {
        fn drop(&mut self) {
            if self.streaming {
                let mut type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
                let _ = ioctl(self.fd, VIDIOC_STREAMOFF, &mut type_);
            }
            for buffer in &self.buffers {
                let _ = unsafe { libv4l2_munmap(buffer.ptr.cast::<c_void>(), buffer.len) };
            }
        }
    }

    fn v4l2_buffer(index: u32) -> V4l2Buffer {
        V4l2Buffer {
            index,
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            bytesused: 0,
            flags: 0,
            field: 0,
            timestamp: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            timecode: V4l2Timecode::default(),
            sequence: 0,
            memory: V4L2_MEMORY_MMAP,
            m_offset: 0,
            m_padding: 0,
            length: 0,
            reserved2: 0,
            request_fd: 0,
        }
    }

    fn ioctl<T>(fd: c_int, request: c_ulong, arg: &mut T) -> io::Result<()> {
        let ret = unsafe { libv4l2_ioctl(fd, request, arg as *mut T) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn best_camera_mode(path: &str) -> Result<CameraMode> {
        let device = Device::open(path).context("failed to open v4l2 device")?;
        let mut modes = Vec::new();

        for fourcc in enum_formats(&device)? {
            let Some(pixel_format) = V4l2PixelFormat::from_fourcc(fourcc) else {
                continue;
            };
            for (width, height) in enum_frame_sizes(&device, fourcc)? {
                let fps = best_fps(&device, fourcc, width, height);
                modes.push(CameraMode {
                    format: pixel_format.fourcc_name().to_string(),
                    width,
                    height,
                    fps,
                });
            }
        }

        modes
            .into_iter()
            .max_by_key(camera_mode_preference)
            .ok_or_else(|| anyhow!("camera did not report usable capture modes"))
    }

    pub(super) fn run_capture(
        config: &Config,
        latest_frame: &Arc<Mutex<LatestCameraFrame>>,
        ready: &std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
    ) -> std::result::Result<(), String> {
        let device = Device::open(&config.device)
            .map_err(|error| format!("failed to open v4l2 device {}: {error}", config.device))?;

        let mut setup_errors = Vec::new();
        let Some((mut stream, frame_config, mut buffer)) = super::preferred_v4l2_formats(config)
            .into_iter()
            .find_map(|pixel_format| {
                let actual = match set_format(&device, config.width, config.height, pixel_format) {
                    Ok(actual) => actual,
                    Err(error) => {
                        setup_errors.push(format!("{pixel_format:?}: {error}"));
                        return None;
                    }
                };
                let Some(actual_format) = V4l2PixelFormat::from_fourcc(actual.pixelformat) else {
                    setup_errors.push(format!(
                        "{pixel_format:?}: device selected unsupported {}",
                        fourcc_name(actual.pixelformat)
                    ));
                    return None;
                };
                if let Err(error) = set_fps(&device, config.fps) {
                    setup_errors.push(format!(
                        "{actual_format:?}: failed to set fps {}: {error}",
                        config.fps
                    ));
                    return None;
                }
                let frame_len = match frame_len(actual.width, actual.height) {
                    Ok(frame_len) => frame_len,
                    Err(error) => {
                        setup_errors.push(format!(
                            "{actual_format:?}: invalid frame size {}x{}: {error}",
                            actual.width, actual.height
                        ));
                        return None;
                    }
                };
                let frame_config = V4l2FrameConfig {
                    pixel_format: actual_format,
                    width: actual.width,
                    height: actual.height,
                    mirror_horizontal: config.mirror_horizontal,
                    frame_len,
                };
                let mut stream = match MmapStream::new(&device, 2) {
                    Ok(stream) => stream,
                    Err(error) => {
                        setup_errors.push(format!(
                            "{actual_format:?}: failed to create mmap stream: {error}"
                        ));
                        return None;
                    }
                };
                let mut buffer = vec![0_u8; frame_config.frame_len];
                match store_next_frame(
                    &mut stream,
                    frame_config,
                    latest_frame,
                    &mut buffer,
                    Duration::from_secs(3),
                ) {
                    Ok(reused) => Some((stream, frame_config, reused)),
                    Err(error) => {
                        setup_errors.push(format!(
                            "{actual_format:?}: failed to read first frame: {error}"
                        ));
                        None
                    }
                }
            })
        else {
            return Err(format!(
                "v4l2 device did not produce a frame for a supported RGB/YUYV/MJPG preview mode near {}x{}{}",
                config.width,
                config.height,
                if setup_errors.is_empty() {
                    String::new()
                } else {
                    format!(": {}", setup_errors.join("; "))
                }
            ));
        };

        let _ = ready.send(Ok(V4l2StreamInfo {
            width: frame_config.width,
            height: frame_config.height,
            input_format: frame_config.pixel_format.input_format(),
        }));

        loop {
            if camera_stream_should_stop(latest_frame) {
                break;
            }
            match store_next_frame(
                &mut stream,
                frame_config,
                latest_frame,
                &mut buffer,
                Duration::from_millis(100),
            ) {
                Ok(reused) => buffer = reused,
                Err(error) if error.kind() == ErrorKind::TimedOut => {}
                Err(error) => return Err(format!("failed to read v4l2 frame: {error}")),
            }
        }
        mark_camera_stream_ended(latest_frame);
        Ok(())
    }

    fn enum_formats(device: &Device) -> io::Result<Vec<u32>> {
        let mut formats = Vec::new();
        for index in 0.. {
            let mut desc = V4l2Fmtdesc {
                index,
                type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
                flags: 0,
                description: [0; 32],
                pixelformat: 0,
                mbus_code: 0,
                reserved: [0; 3],
            };
            match device.ioctl(VIDIOC_ENUM_FMT, &mut desc) {
                Ok(()) => formats.push(desc.pixelformat),
                Err(error) if error.raw_os_error() == Some(libc::EINVAL) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(formats)
    }

    fn enum_frame_sizes(device: &Device, fourcc: u32) -> io::Result<Vec<(u32, u32)>> {
        let mut sizes = Vec::new();
        for index in 0.. {
            let mut frame_size = V4l2FrmSizeEnum {
                index,
                pixel_format: fourcc,
                type_: 0,
                size: V4l2FrmSizeUnion {
                    discrete: V4l2FrmSizeDiscrete {
                        width: 0,
                        height: 0,
                    },
                },
                reserved: [0; 2],
            };
            match device.ioctl(VIDIOC_ENUM_FRAMESIZES, &mut frame_size) {
                Ok(()) => match frame_size.type_ {
                    V4L2_FRMSIZE_TYPE_DISCRETE => {
                        let discrete = unsafe { frame_size.size.discrete };
                        sizes.push((discrete.width, discrete.height));
                    }
                    V4L2_FRMSIZE_TYPE_STEPWISE => {
                        let stepwise = unsafe { frame_size.size.stepwise };
                        sizes.push((stepwise.max_width, stepwise.max_height));
                    }
                    _ => {}
                },
                Err(error) if error.raw_os_error() == Some(libc::EINVAL) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(sizes)
    }

    fn best_fps(device: &Device, fourcc: u32, width: u32, height: u32) -> u32 {
        let mut best = None;
        for index in 0.. {
            let mut frame_interval = V4l2FrmIvalEnum {
                index,
                pixel_format: fourcc,
                width,
                height,
                type_: 0,
                interval: V4l2FrmIvalUnion {
                    discrete: V4l2Fract {
                        numerator: 0,
                        denominator: 0,
                    },
                },
                reserved: [0; 2],
            };
            match device.ioctl(VIDIOC_ENUM_FRAMEINTERVALS, &mut frame_interval) {
                Ok(()) => {
                    let fps = match frame_interval.type_ {
                        V4L2_FRMIVAL_TYPE_DISCRETE => {
                            let discrete = unsafe { frame_interval.interval.discrete };
                            super::fps_from_interval(discrete.numerator, discrete.denominator)
                        }
                        V4L2_FRMIVAL_TYPE_STEPWISE => {
                            let stepwise = unsafe { frame_interval.interval.stepwise };
                            super::fps_from_interval(
                                stepwise.min.numerator,
                                stepwise.min.denominator,
                            )
                        }
                        _ => None,
                    };
                    best = best.max(fps);
                }
                Err(error) if error.raw_os_error() == Some(libc::EINVAL) => break,
                Err(_) => break,
            }
        }
        best.unwrap_or(0)
    }

    fn set_format(
        device: &Device,
        width: u32,
        height: u32,
        pixel_format: V4l2PixelFormat,
    ) -> io::Result<V4l2PixFormat> {
        let mut format = V4l2Format {
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            union_align: 0,
            pix: V4l2PixFormat {
                width,
                height,
                pixelformat: pixel_format.fourcc(),
                field: 0,
                bytesperline: 0,
                sizeimage: 0,
                colorspace: 0,
                priv_: 0,
                flags: 0,
                ycbcr_enc: 0,
                quantization: 0,
                xfer_func: 0,
            },
            padding: [0; 152],
        };
        device.ioctl(VIDIOC_S_FMT, &mut format)?;
        Ok(format.pix)
    }

    fn set_fps(device: &Device, fps: u32) -> io::Result<()> {
        if fps == 0 {
            return Ok(());
        }
        let mut params = V4l2StreamParm {
            type_: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            capture: V4l2StreamParmCapture {
                capability: 0,
                capturemode: 0,
                timeperframe: V4l2Fract {
                    numerator: 1,
                    denominator: fps,
                },
                extendedmode: 0,
                readbuffers: 0,
                reserved: [0; 4],
            },
            padding: [0; 160],
        };
        device.ioctl(VIDIOC_S_PARM, &mut params)
    }

    fn store_next_frame(
        stream: &mut MmapStream,
        config: V4l2FrameConfig,
        latest_frame: &Arc<Mutex<LatestCameraFrame>>,
        buffer: &mut [u8],
        timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        let (frame, index) = stream.next_frame(timeout)?;
        let decode_result = decode_v4l2_frame(
            config.pixel_format,
            frame,
            config.width,
            config.height,
            buffer,
        )
        .map_err(io::Error::other);
        stream.queue_buffer(index)?;
        decode_result?;
        if config.mirror_horizontal {
            mirror_rgb24_in_place(buffer, config.width, config.height);
        }
        Ok(store_latest_frame(
            latest_frame,
            buffer.to_vec(),
            config.frame_len,
        ))
    }

    fn fourcc_name(fourcc: u32) -> String {
        let bytes = fourcc.to_le_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    const _: () = {
        assert!(mem::size_of::<V4l2Fmtdesc>() == 64);
        assert!(mem::size_of::<V4l2Format>() == 208);
        assert!(mem::size_of::<V4l2StreamParm>() == 204);
        assert!(mem::size_of::<V4l2RequestBuffers>() == 20);
        assert!(mem::size_of::<V4l2Buffer>() == 88);
        assert!(mem::size_of::<V4l2FrmSizeEnum>() == 44);
        assert!(mem::size_of::<V4l2FrmIvalEnum>() == 52);
    };
}

fn set_camera_stream_text(state: &Arc<Mutex<String>>, text: &str) {
    if let Ok(mut state) = state.lock() {
        *state = text.to_string();
    }
}

fn ffmpeg_video_filter(config: &Config) -> String {
    let mirror = if config.mirror_horizontal {
        "hflip,"
    } else {
        ""
    };
    format!(
        "{mirror}scale={}:{}:force_original_aspect_ratio=increase,crop={}:{}:(iw-ow)/2:(ih-oh)/2,format=rgb24",
        config.width, config.height, config.width, config.height
    )
}

#[cfg(test)]
mod tests {
    use std::{env, fs, thread, time::SystemTime};

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
    fn v4l2_format_preference_honors_probed_mjpeg_mode_first() {
        let config = Config {
            input_format: Some("mjpeg".to_string()),
            ..Config::default()
        };

        assert_eq!(
            preferred_v4l2_formats(&config),
            vec![
                V4l2PixelFormat::Mjpeg,
                V4l2PixelFormat::Yuyv,
                V4l2PixelFormat::Rgb24,
            ]
        );
    }

    #[test]
    fn v4l2_format_preference_tries_mjpeg_before_yuyv_without_probe() {
        let config = Config::default();

        assert_eq!(
            preferred_v4l2_formats(&config),
            vec![
                V4l2PixelFormat::Mjpeg,
                V4l2PixelFormat::Yuyv,
                V4l2PixelFormat::Rgb24
            ]
        );
    }

    #[test]
    fn yuyv_conversion_outputs_rgb_pairs() {
        let frame = [16, 128, 235, 128];
        let mut out = [0_u8; 6];

        convert_yuyv_to_rgb24(&frame, 2, 1, &mut out).expect("yuyv should convert");

        assert_eq!(out, [0, 0, 0, 255, 255, 255]);
    }

    #[test]
    fn rgb_mirror_flips_rows_in_place() {
        let mut frame = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

        mirror_rgb24_in_place(&mut frame, 2, 2);

        assert_eq!(frame, vec![4, 5, 6, 1, 2, 3, 10, 11, 12, 7, 8, 9]);
    }

    #[test]
    fn camera_stream_args_cap_input_queue_before_device() {
        let config = Config::default();
        let args = camera_stream_ffmpeg_args(&config);
        let queue_index = args
            .iter()
            .position(|arg| arg == "-thread_queue_size")
            .expect("input queue should be capped");
        let input_index = args
            .iter()
            .position(|arg| arg == "-i")
            .expect("camera input should be present");

        assert_eq!(args.get(queue_index + 1).map(String::as_str), Some("1"));
        assert!(queue_index < input_index);
    }

    #[test]
    fn ffmpeg_filter_adds_hflip_when_horizontal_mirror_is_enabled() {
        let mut config = Config::default();

        assert!(ffmpeg_video_filter(&config).starts_with("hflip,"));

        config.mirror_horizontal = false;
        let filter = ffmpeg_video_filter(&config);
        assert!(filter.starts_with("scale="));
        assert!(filter.contains("force_original_aspect_ratio=increase,crop="));
        assert!(!filter.contains("fps="));
    }

    #[test]
    fn best_camera_mode_prefers_largest_mjpg_mode() {
        let output = r#"
            ioctl: VIDIOC_ENUM_FMT
                Type: Video Capture

                [0]: 'MJPG' (Motion-JPEG, compressed)
                    Size: Discrete 1920x1080
                        Interval: Discrete 0.033s (30.000 fps)
                    Size: Discrete 1280x960
                        Interval: Discrete 0.033s (30.000 fps)
                [1]: 'YUYV' (YUYV 4:2:2)
                    Size: Discrete 640x480
                        Interval: Discrete 0.033s (30.000 fps)
        "#;

        assert_eq!(
            parse_best_camera_mode(output),
            Some(CameraMode {
                format: "MJPG".to_string(),
                width: 1920,
                height: 1080,
                fps: 30,
            })
        );
    }

    #[test]
    fn square_thumbnail_returns_square_rgb_buffer() {
        let frame = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];

        let thumbnail = square_thumbnail(&frame, 2, 2, 4);

        assert_eq!(thumbnail.len(), 4 * 4 * RAW_RGB_BYTES_PER_PIXEL);
    }

    #[test]
    fn square_thumbnail_crops_instead_of_letterboxing() {
        let frame = vec![255; 4 * 2 * RAW_RGB_BYTES_PER_PIXEL];

        let thumbnail = square_thumbnail(&frame, 4, 2, 4);

        assert!(thumbnail.iter().all(|&byte| byte == 255));
    }

    #[test]
    fn video_path_uses_mp4_extension() {
        let path =
            video_path(Path::new("/tmp/camilo-camera")).expect("video path should be generated");

        assert_eq!(path.extension().and_then(OsStr::to_str), Some("mp4"));
    }

    #[test]
    fn recording_args_enable_default_pulse_audio() {
        let config = Config::default();
        let args = recording_ffmpeg_args(
            &config,
            Path::new("/tmp/camilo.mp4"),
            "1920x1080",
            "30",
            RecordingAudio::Input,
        );

        assert!(
            args.windows(4)
                .any(|window| window == ["-f", "pulse", "-i", "default"])
        );
        assert!(
            args.windows(4)
                .any(|window| window == ["-c:a", "aac", "-b:a", "128k"])
        );
        assert!(args.iter().any(|arg| arg == "-shortest"));
        assert!(!args.iter().any(|arg| arg == "-an"));
    }

    #[test]
    fn recording_args_allow_disabling_audio() {
        let config = Config {
            audio: false,
            ..Config::default()
        };
        let args = recording_ffmpeg_args(
            &config,
            Path::new("/tmp/camilo.mp4"),
            "1920x1080",
            "30",
            RecordingAudio::Disabled,
        );

        assert!(args.iter().any(|arg| arg == "-an"));
        assert!(
            !args
                .windows(4)
                .any(|window| window == ["-f", "pulse", "-i", "default"])
        );
        assert!(!args.iter().any(|arg| arg == "-c:a"));
    }

    #[test]
    fn recording_args_use_silent_aac_for_audio_fallback() {
        let config = Config::default();
        let args = recording_ffmpeg_args(
            &config,
            Path::new("/tmp/camilo.mp4"),
            "1920x1080",
            "30",
            RecordingAudio::Silent,
        );

        assert!(args.windows(4).any(|window| window
            == [
                "-f",
                "lavfi",
                "-i",
                "anullsrc=channel_layout=stereo:sample_rate=48000"
            ]));
        assert!(
            args.windows(4)
                .any(|window| window == ["-c:a", "aac", "-b:a", "128k"])
        );
        assert!(args.iter().any(|arg| arg == "-shortest"));
        assert!(!args.iter().any(|arg| arg == "-an"));
    }

    #[test]
    fn recording_args_accept_explicit_audio_backend_prefix() {
        let config = Config {
            audio_input: "alsa:hw:0".to_string(),
            ..Config::default()
        };
        let args = recording_ffmpeg_args(
            &config,
            Path::new("/tmp/camilo.mp4"),
            "1920x1080",
            "30",
            RecordingAudio::Input,
        );

        assert!(
            args.windows(4)
                .any(|window| window == ["-f", "alsa", "-i", "hw:0"])
        );
    }

    #[test]
    fn latest_image_path_uses_newest_supported_image() {
        let dir = env::temp_dir().join(format!(
            "camilo-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be valid")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("test dir should be created");

        let old = dir.join("old.jpg");
        let ignored = dir.join("newer.txt");
        let new = dir.join("new.png");
        fs::write(&old, b"old").expect("old image should be written");
        thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&ignored, b"ignored").expect("ignored file should be written");
        thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&new, b"new").expect("new image should be written");

        assert_eq!(latest_image_path(&dir), Some(new));

        fs::remove_dir_all(dir).expect("test dir should be removed");
    }

    #[test]
    fn latest_image_path_ignores_missing_directory() {
        let path = env::temp_dir().join("camilo-definitely-missing-camera-dir");

        assert_eq!(latest_image_path(&path), None);
    }
}
