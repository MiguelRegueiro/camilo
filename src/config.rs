use std::{env, fs, io::ErrorKind, path::PathBuf};

use anyhow::{Context, Result, anyhow, bail};

pub(crate) const DEFAULT_WIDTH: u32 = 1920;
pub(crate) const DEFAULT_HEIGHT: u32 = 1080;
pub(crate) const DEFAULT_FPS: u32 = 30;
pub(crate) const DEFAULT_DEVICE: &str = "/dev/video0";

#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub(crate) device: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fps: u32,
    pub(crate) input_format: Option<String>,
    pub(crate) force: bool,
    pub(crate) mirror_horizontal: bool,
    pub(crate) camera_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device: DEFAULT_DEVICE.to_string(),
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            fps: DEFAULT_FPS,
            input_format: None,
            force: false,
            mirror_horizontal: false,
            camera_dir: default_camera_dir(),
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
            "width" => config.width = parse_config_u32(value, line_number, key)?,
            "height" => config.height = parse_config_u32(value, line_number, key)?,
            "fps" => config.fps = parse_config_u32(value, line_number, key)?,
            "force" => config.force = parse_config_bool(value, line_number, key)?,
            "mirror_horizontal" => {
                config.mirror_horizontal = parse_config_bool(value, line_number, key)?;
            }
            "camera_dir" => config.camera_dir = parse_config_path(value, line_number, key)?,
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

fn default_camera_dir() -> PathBuf {
    env::var_os("HOME")
        .filter(|path| !path.as_os_str().is_empty())
        .map(|home| PathBuf::from(home).join("Pictures").join("Camera"))
        .unwrap_or_else(|| PathBuf::from("Camera"))
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

        apply_config_text(&mut config, "mirror_horizontal = true\n")
            .expect("config file should parse");

        assert!(config.mirror_horizontal);
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
                camera_dir = "/tmp/camera"
            "#,
        )
        .expect("config file should parse");

        assert_eq!(config.device, "/dev/video1");
        assert_eq!(config.width, 800);
        assert_eq!(config.height, 450);
        assert_eq!(config.fps, 60);
        assert_eq!(config.camera_dir, PathBuf::from("/tmp/camera"));
    }

    #[test]
    fn config_file_rejects_unknown_keys() {
        let mut config = Config::default();

        let error = apply_config_text(&mut config, "unknown = true\n")
            .expect_err("unknown key should fail");

        assert!(error.to_string().contains("unknown config key"));
    }
}
