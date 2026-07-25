use std::path::{Path, PathBuf};

use anyhow::Result;
use time::OffsetDateTime;

pub(super) fn photo_path(picture_dir: &Path) -> Result<PathBuf> {
    timestamped_media_path(picture_dir, "Photo from", "jpg")
}

pub(super) fn video_path(video_dir: &Path) -> Result<PathBuf> {
    timestamped_media_path(video_dir, "Recording from", "mp4")
}

fn timestamped_media_path(dir: &Path, prefix: &str, extension: &str) -> Result<PathBuf> {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let timestamp = media_timestamp(now);
    Ok(dir.join(format!("{prefix} {timestamp}.{extension}")))
}

fn media_timestamp(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02}.{:06}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.microsecond()
    )
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;
    use time::{Date, Month, PrimitiveDateTime, Time};

    #[test]
    fn photo_path_uses_gnome_style_name() {
        let path = photo_path(Path::new("/tmp/camilo-pictures")).expect("path should be generated");
        let name = path.file_name().and_then(OsStr::to_str).unwrap();

        assert!(path.starts_with("/tmp/camilo-pictures"));
        assert!(name.starts_with("Photo from "));
        assert!(name.ends_with(".jpg"));
    }

    #[test]
    fn video_path_uses_gnome_style_name() {
        let path = video_path(Path::new("/tmp/camilo-videos")).expect("path should be generated");
        let name = path.file_name().and_then(OsStr::to_str).unwrap();

        assert!(path.starts_with("/tmp/camilo-videos"));
        assert!(name.starts_with("Recording from "));
        assert!(name.ends_with(".mp4"));
    }

    #[test]
    fn media_timestamp_uses_gnome_style_shape() {
        let timestamp = media_timestamp(
            PrimitiveDateTime::new(
                Date::from_calendar_date(2025, Month::November, 24).unwrap(),
                Time::from_hms_micro(11, 0, 27, 764326).unwrap(),
            )
            .assume_utc(),
        );

        assert_eq!(timestamp, "2025-11-24 11-00-27.764326");
    }
}
