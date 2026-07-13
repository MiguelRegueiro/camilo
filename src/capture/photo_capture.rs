use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::Config;

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

fn capture_path(camera_dir: &Path) -> Result<PathBuf> {
    timestamped_media_path(camera_dir, "capture", "jpg")
}

fn timestamped_media_path(camera_dir: &Path, prefix: &str, extension: &str) -> Result<PathBuf> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    let stem = format!("{prefix}-{}-{:09}", now.as_secs(), now.subsec_nanos());
    Ok(camera_dir.join(format!("{stem}.{extension}")))
}
