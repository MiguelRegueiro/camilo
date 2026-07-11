use std::{
    ffi::OsStr,
    fs,
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::Config;

pub(crate) const RAW_RGB_BYTES_PER_PIXEL: usize = 3;
pub(crate) const THUMBNAIL_SIZE: u32 = 160;

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
                "-framerate",
                &framerate,
                "-i",
                "pipe:0",
                "-an",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-movflags",
                "+faststart",
                "-y",
            ])
            .arg(&path)
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

pub(crate) struct CameraStream {
    child: Child,
    stdout: ChildStdout,
    stderr: Arc<Mutex<String>>,
    stderr_thread: Option<thread::JoinHandle<()>>,
}

impl CameraStream {
    pub(crate) fn spawn(config: &Config) -> Result<Self> {
        let video_filter = ffmpeg_video_filter(config);
        let input_size = format!("{}x{}", config.width, config.height);
        let framerate = config.fps.to_string();

        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-fflags",
                "nobuffer",
                "-flags",
                "low_delay",
                "-f",
                "v4l2",
                "-framerate",
                &framerate,
                "-video_size",
                &input_size,
                "-i",
                &config.device,
                "-an",
                "-sn",
                "-dn",
                "-vf",
                &video_filter,
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start ffmpeg; is it installed and in PATH?")?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture ffmpeg stdout"))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture ffmpeg stderr"))?;
        let (stderr, stderr_thread) = read_stderr_async(stderr_pipe);

        Ok(Self {
            child,
            stdout,
            stderr,
            stderr_thread: Some(stderr_thread),
        })
    }

    pub(crate) fn read_frame(&mut self, frame: &mut [u8]) -> io::Result<bool> {
        match self.stdout.read_exact(frame) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => Ok(false),
            Err(error) => Err(error),
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
        "{mirror}scale={}:{}:force_original_aspect_ratio=increase,crop={}:{}:(iw-ow)/2:(ih-oh)/2,fps={},format=rgb24",
        config.width, config.height, config.width, config.height, config.fps
    )
}

#[cfg(test)]
mod tests {
    use std::{env, fs, thread, time::SystemTime};

    use super::*;

    #[test]
    fn ffmpeg_filter_adds_hflip_when_horizontal_mirror_is_enabled() {
        let mut config = Config::default();

        assert!(!ffmpeg_video_filter(&config).starts_with("hflip,"));

        config.mirror_horizontal = true;
        let filter = ffmpeg_video_filter(&config);
        assert!(filter.starts_with("hflip,scale="));
        assert!(filter.contains("force_original_aspect_ratio=increase,crop="));
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
            video_path(Path::new("/tmp/lumi-camera")).expect("video path should be generated");

        assert_eq!(path.extension().and_then(OsStr::to_str), Some("mp4"));
    }

    #[test]
    fn latest_image_path_uses_newest_supported_image() {
        let dir = env::temp_dir().join(format!(
            "lumi-test-{}",
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
        let path = env::temp_dir().join("lumi-definitely-missing-camera-dir");

        assert_eq!(latest_image_path(&path), None);
    }
}
