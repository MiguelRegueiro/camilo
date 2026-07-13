use std::{
    fs,
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::Config;

use super::ffmpeg::read_stderr_async;

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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

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
}
