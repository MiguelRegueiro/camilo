use std::{env, fs, io::ErrorKind, path::PathBuf};

use anyhow::{Context, Result, anyhow, bail};

pub(crate) const DEFAULT_WIDTH: u32 = 1920;
pub(crate) const DEFAULT_HEIGHT: u32 = 1080;
pub(crate) const DEFAULT_FPS: u32 = 30;
pub(crate) const DEFAULT_DEVICE: &str = "/dev/video0";
pub(crate) const DEFAULT_AUDIO_INPUT: &str = "default";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewBackend {
    Auto,
    V4l2,
    Ffmpeg,
}

impl PreviewBackend {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "v4l2" => Some(Self::V4l2),
            "ffmpeg" => Some(Self::Ffmpeg),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) device: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) width_set: bool,
    pub(crate) height_set: bool,
    pub(crate) fps: u32,
    pub(crate) fps_set: bool,
    pub(crate) input_format: Option<String>,
    pub(crate) preview_backend: PreviewBackend,
    pub(crate) force: bool,
    pub(crate) camera_info: bool,
    pub(crate) mirror_horizontal: bool,
    pub(crate) picture_dir: PathBuf,
    pub(crate) video_dir: PathBuf,
    pub(crate) audio: bool,
    pub(crate) audio_input: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: DEFAULT_DEVICE.to_string(),
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            width_set: false,
            height_set: false,
            fps: DEFAULT_FPS,
            fps_set: false,
            input_format: None,
            preview_backend: PreviewBackend::Auto,
            force: false,
            camera_info: false,
            mirror_horizontal: true,
            picture_dir: default_picture_dir(),
            video_dir: default_video_dir(),
            audio: true,
            audio_input: DEFAULT_AUDIO_INPUT.to_string(),
        }
    }
}

impl Config {
    pub(crate) fn load() -> Result<Self> {
        let mut config = Self::default();
        load_config_file(&mut config)?;
        Ok(config)
    }
}

fn load_config_file(config: &mut Config) -> Result<()> {
    let Some(path) = config_path() else {
        return Ok(());
    };

    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read config file {}", path.display()));
        }
    };

    apply_config_text(config, &text)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    Ok(())
}

fn apply_config_text(config: &mut Config, text: &str) -> Result<()> {
    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = strip_config_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("line {line_number}: expected `key = value`"))?;
        let key = key.trim();
        let value = value.trim();

        match key {
            "device" => config.device = parse_config_string(value, line_number, key)?,
            "width" => {
                config.width = parse_config_u32(value, line_number, key)?;
                config.width_set = true;
            }
            "height" => {
                config.height = parse_config_u32(value, line_number, key)?;
                config.height_set = true;
            }
            "fps" => {
                config.fps = parse_config_u32(value, line_number, key)?;
                config.fps_set = true;
            }
            "preview_backend" => {
                config.preview_backend = parse_preview_backend(value, line_number, key)?;
            }
            "force" => config.force = parse_config_bool(value, line_number, key)?,
            "mirror_horizontal" => {
                config.mirror_horizontal = parse_config_bool(value, line_number, key)?;
            }
            "picture_dir" => config.picture_dir = parse_config_path(value, line_number, key)?,
            "video_dir" => config.video_dir = parse_config_path(value, line_number, key)?,
            "audio" => config.audio = parse_config_bool(value, line_number, key)?,
            "audio_input" => config.audio_input = parse_config_string(value, line_number, key)?,
            "" => bail!("line {line_number}: empty config key"),
            _ => bail!("line {line_number}: unknown config key `{key}`"),
        }
    }
    Ok(())
}

fn strip_config_comment(line: &str) -> &str {
    line.split_once('#').map_or(line, |(value, _)| value)
}

fn parse_config_string(value: &str, line_number: usize, key: &str) -> Result<String> {
    let Some(body) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        bail!("line {line_number}: `{key}` expects a quoted string");
    };

    Ok(body.to_string())
}

fn parse_config_path(value: &str, line_number: usize, key: &str) -> Result<PathBuf> {
    Ok(expand_home_path(&parse_config_string(
        value,
        line_number,
        key,
    )?))
}

fn parse_preview_backend(value: &str, line_number: usize, key: &str) -> Result<PreviewBackend> {
    let value = parse_config_string(value, line_number, key)?;
    PreviewBackend::parse(&value)
        .ok_or_else(|| anyhow!("line {line_number}: `{key}` expects auto, v4l2, or ffmpeg"))
}

fn parse_config_u32(value: &str, line_number: usize, key: &str) -> Result<u32> {
    value
        .parse::<u32>()
        .with_context(|| format!("line {line_number}: `{key}` expects a positive integer"))
}

fn parse_config_bool(value: &str, line_number: usize, key: &str) -> Result<bool> {
    value
        .parse::<bool>()
        .with_context(|| format!("line {line_number}: `{key}` expects true or false"))
}

fn config_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.as_os_str().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.as_os_str().is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .map(|dir| dir.join("camilo").join("config.toml"))
}

fn default_picture_dir() -> PathBuf {
    default_xdg_media_dir("PICTURES", "Pictures")
}

fn default_video_dir() -> PathBuf {
    default_xdg_media_dir("VIDEOS", "Videos")
}

fn default_xdg_media_dir(kind: &str, fallback_name: &str) -> PathBuf {
    xdg_user_dir(kind)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.as_os_str().is_empty())
                .map(|home| PathBuf::from(home).join(fallback_name))
        })
        .map(|dir| dir.join("Camera"))
        .unwrap_or_else(|| PathBuf::from(fallback_name).join("Camera"))
}

fn xdg_user_dir(kind: &str) -> Option<PathBuf> {
    let key = format!("XDG_{kind}_DIR");
    env::var_os(&key)
        .filter(|path| !path.as_os_str().is_empty())
        .map(PathBuf::from)
        .or_else(|| xdg_user_dir_from_config(&key))
}

fn xdg_user_dir_from_config(key: &str) -> Option<PathBuf> {
    let path = env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.as_os_str().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|path| !path.as_os_str().is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })?
        .join("user-dirs.dirs");
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|line| parse_user_dir_line(line, key))
        .next()
}

fn parse_user_dir_line(line: &str, key: &str) -> Option<PathBuf> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (line_key, value) = line.split_once('=')?;
    if line_key.trim() != key {
        return None;
    }
    let path = parse_config_string(value.trim(), 0, key).ok()?;
    Some(expand_xdg_user_dir_path(&path))
}

fn expand_xdg_user_dir_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("$HOME/") {
        if let Some(home) = env::var_os("HOME").filter(|home| !home.as_os_str().is_empty()) {
            return PathBuf::from(home).join(rest);
        }
    } else if path == "$HOME"
        && let Some(home) = env::var_os("HOME").filter(|home| !home.as_os_str().is_empty())
    {
        return PathBuf::from(home);
    }

    expand_home_path(path)
}

pub(crate) fn expand_home_path(path: &str) -> PathBuf {
    let home = env::var_os("HOME").filter(|home| !home.as_os_str().is_empty());
    if path == "~" {
        if let Some(home) = home {
            return PathBuf::from(home);
        }
    } else if let (Some(rest), Some(home)) = (path.strip_prefix("~/"), home) {
        return PathBuf::from(home).join(rest);
    }

    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn config_file_applies_horizontal_mirror() {
        let mut config = Config::default();

        apply_config_text(&mut config, "mirror_horizontal = false\n")
            .expect("config file should parse");

        assert!(!config.mirror_horizontal);
        assert_eq!(config.device, DEFAULT_DEVICE);
    }

    #[test]
    fn config_file_parses_comments_strings_and_numbers() {
        let mut config = Config::default();

        apply_config_text(
            &mut config,
            r#"
                # camera settings
                device = "/dev/video1" # inline comment
                width = 800
                height = 450
                fps = 60
                preview_backend = "v4l2"
                picture_dir = "/tmp/pictures"
                video_dir = "/tmp/videos"
                audio = false
                audio_input = "pulse:alsa_input.usb-Test_Mic"
            "#,
        )
        .expect("config file should parse");

        assert_eq!(config.device, "/dev/video1");
        assert_eq!(config.width, 800);
        assert_eq!(config.height, 450);
        assert!(config.width_set);
        assert!(config.height_set);
        assert_eq!(config.fps, 60);
        assert!(config.fps_set);
        assert_eq!(config.preview_backend, PreviewBackend::V4l2);
        assert_eq!(config.picture_dir, PathBuf::from("/tmp/pictures"));
        assert_eq!(config.video_dir, PathBuf::from("/tmp/videos"));
        assert!(!config.audio);
        assert_eq!(config.audio_input, "pulse:alsa_input.usb-Test_Mic");
    }

    #[test]
    fn config_file_rejects_removed_camera_dir() {
        let mut config = Config::default();

        let error = apply_config_text(&mut config, "camera_dir = \"/tmp/camera\"\n")
            .expect_err("removed camera_dir key should fail");

        assert!(
            error
                .to_string()
                .contains("unknown config key `camera_dir`")
        );
    }

    #[test]
    fn xdg_user_dir_lines_expand_home() {
        let parsed = parse_user_dir_line("XDG_VIDEOS_DIR=\"$HOME/Clips\"", "XDG_VIDEOS_DIR")
            .expect("line should parse");

        if let Some(home) = env::var_os("HOME").filter(|home| !home.as_os_str().is_empty()) {
            assert_eq!(parsed, PathBuf::from(home).join("Clips"));
        }
    }

    #[test]
    fn config_file_rejects_unknown_keys() {
        let mut config = Config::default();

        let error = apply_config_text(&mut config, "unknown = true\n")
            .expect_err("unknown key should fail");

        assert!(error.to_string().contains("unknown config key"));
    }
}
