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

use crate::config::Config;

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
    let output = Command::new("v4l2-ctl")
        .arg(format!("--device={device}"))
        .arg("--list-formats-ext")
        .output()
        .context("failed to query camera formats with v4l2-ctl")?;
    if !output.status.success() {
        bail!("v4l2-ctl failed to query camera formats");
    }
    parse_best_camera_mode(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| anyhow!("camera did not report usable capture modes"))
}

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

    modes.into_iter().max_by_key(|mode| {
        (
            mode.width as u64 * mode.height as u64,
            mode.fps,
            u8::from(mode.format == "MJPG"),
        )
    })
}

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
        other => other,
    }
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

    if config.audio {
        let (backend, input) = ffmpeg_audio_input(&config.audio_input);
        args.extend([
            "-f".to_string(),
            backend.to_string(),
            "-i".to_string(),
            input.to_string(),
        ]);
    } else {
        args.push("-an".to_string());
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

    if config.audio {
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

fn ffmpeg_audio_input(input: &str) -> (&str, &str) {
    input
        .split_once(':')
        .filter(|(backend, device)| is_supported_audio_backend(backend) && !device.is_empty())
        .unwrap_or(("pulse", input))
}

fn is_supported_audio_backend(backend: &str) -> bool {
    matches!(backend, "pulse" | "alsa" | "oss" | "avfoundation")
}

pub(crate) struct VideoRecording {
    child: Child,
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<String>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
}

impl VideoRecording {
    pub(crate) fn start(config: &Config) -> Result<Self> {
        fs::create_dir_all(&config.camera_dir).with_context(|| {
            format!(
                "failed to create camera directory {}",
                config.camera_dir.display()
            )
        })?;
        let path = video_path(&config.camera_dir)?;
        let size = format!("{}x{}", config.width, config.height);
        let framerate = config.fps.to_string();

        let args = recording_ffmpeg_args(config, &path, &size, &framerate);
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
        })
    }

    pub(crate) fn write_frame(&mut self, frame: &[u8]) -> Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("video recording input is closed"))?;
        stdin
            .write_all(frame)
            .context("failed to send frame to video encoder")
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

    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }
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

pub(crate) struct CameraStream {
    child: Child,
    latest_frame: Arc<Mutex<LatestCameraFrame>>,
    delivered_serial: u64,
    frame_thread: Option<thread::JoinHandle<()>>,
    stderr: Arc<Mutex<String>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
}

impl CameraStream {
    pub(crate) fn spawn(config: &Config) -> Result<Self> {
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

    pub(crate) fn read_latest_frame(&mut self, frame: &mut [u8]) -> io::Result<CameraFrameStatus> {
        let mut state = self
            .latest_frame
            .lock()
            .map_err(|_| io::Error::other("camera frame state is poisoned"))?;
        if state.serial != self.delivered_serial {
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
            self.delivered_serial = state.serial;
            Ok(CameraFrameStatus::NewFrame)
        } else if let Some(error) = state.error.take() {
            Err(io::Error::other(error))
        } else if state.ended {
            Ok(CameraFrameStatus::Ended)
        } else {
            Ok(CameraFrameStatus::NoFrame)
        }
    }

    pub(crate) fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    pub(crate) fn stop(&mut self) {
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

impl Drop for CameraStream {
    fn drop(&mut self) {
        self.stop();
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

        assert!(!ffmpeg_video_filter(&config).starts_with("hflip,"));

        config.mirror_horizontal = true;
        let filter = ffmpeg_video_filter(&config);
        assert!(filter.starts_with("hflip,scale="));
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
        let args = recording_ffmpeg_args(&config, Path::new("/tmp/camilo.mp4"), "1920x1080", "30");

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
        let args = recording_ffmpeg_args(&config, Path::new("/tmp/camilo.mp4"), "1920x1080", "30");

        assert!(args.iter().any(|arg| arg == "-an"));
        assert!(
            !args
                .windows(4)
                .any(|window| window == ["-f", "pulse", "-i", "default"])
        );
        assert!(!args.iter().any(|arg| arg == "-c:a"));
    }

    #[test]
    fn recording_args_accept_explicit_audio_backend_prefix() {
        let config = Config {
            audio_input: "alsa:hw:0".to_string(),
            ..Config::default()
        };
        let args = recording_ffmpeg_args(&config, Path::new("/tmp/camilo.mp4"), "1920x1080", "30");

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
