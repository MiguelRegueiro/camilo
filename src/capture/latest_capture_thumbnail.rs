use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

use super::rgb_frame::{RAW_RGB_BYTES_PER_PIXEL, frame_len};

pub(crate) const THUMBNAIL_SIZE: u32 = 160;

#[derive(Clone, Debug)]
pub(crate) struct CaptureThumbnail {
    pub(crate) path: PathBuf,
    pub(crate) frame: Vec<u8>,
}

pub(crate) fn latest_capture_thumbnail(picture_dir: &Path, size: u32) -> Option<CaptureThumbnail> {
    let path = latest_image_path(picture_dir)?;
    let frame = load_image_thumbnail(&path, size).ok()?;
    Some(CaptureThumbnail { path, frame })
}

pub(crate) fn latest_image_path(picture_dir: &Path) -> Option<PathBuf> {
    capture_image_paths(picture_dir).pop()
}

pub(crate) fn capture_image_paths(picture_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(picture_dir) else {
        return Vec::new();
    };
    let mut captures = entries
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
        .collect::<Vec<_>>();
    captures.sort();
    captures.into_iter().map(|(_, path)| path).collect()
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
    load_image_with_filter(
        path,
        size,
        size,
        &format!(
            "scale={size}:{size}:force_original_aspect_ratio=increase,crop={size}:{size}:(iw-ow)/2:(ih-oh)/2,format=rgb24"
        ),
    )
}

pub(crate) fn load_gallery_image(path: &Path, width: u32, height: u32) -> Result<Vec<u8>> {
    let width = width.max(1);
    let height = height.max(1);
    load_image_with_filter(
        path,
        width,
        height,
        &format!(
            "scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black,format=rgb24"
        ),
    )
}

fn load_image_with_filter(
    path: &Path,
    output_width: u32,
    output_height: u32,
    filter: &str,
) -> Result<Vec<u8>> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-vf", filter, "-frames:v", "1", "-f", "rawvideo", "pipe:1"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("failed to load thumbnail from {}", path.display()))?;

    if !output.status.success() {
        bail!("ffmpeg could not decode {}", path.display());
    }

    let expected_len = frame_len(output_width, output_height)?;
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

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        process::Command,
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

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

        assert_eq!(capture_image_paths(&dir), vec![old, new.clone()]);
        assert_eq!(latest_image_path(&dir), Some(new));

        fs::remove_dir_all(dir).expect("test dir should be removed");
    }

    #[test]
    fn latest_image_path_ignores_missing_directory() {
        let path = env::temp_dir().join("camilo-definitely-missing-camera-dir");

        assert_eq!(latest_image_path(&path), None);
    }

    #[test]
    fn gallery_image_preserves_aspect_ratio_with_black_letterboxing() {
        let dir = env::temp_dir().join(format!(
            "camilo-gallery-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be valid")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("test dir should be created");
        let source = dir.join("wide.png");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=4x2",
                "-frames:v",
                "1",
                "-y",
            ])
            .arg(&source)
            .status()
            .expect("ffmpeg should generate a gallery fixture");
        assert!(status.success());

        let frame = load_gallery_image(&source, 4, 4).expect("gallery image should decode");
        let pixel = |x: usize, y: usize| &frame[(y * 4 + x) * 3..(y * 4 + x + 1) * 3];

        assert_eq!(frame.len(), 4 * 4 * RAW_RGB_BYTES_PER_PIXEL);
        assert!(pixel(2, 0).iter().all(|&channel| channel < 8));
        assert!(pixel(2, 1)[0] > 200);
        assert!(pixel(2, 1)[1] < 30);
        assert!(pixel(2, 1)[2] < 30);

        fs::remove_dir_all(dir).expect("test dir should be removed");
    }
}
