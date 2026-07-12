use std::env;

use anyhow::{Context, Result, anyhow, bail};

use crate::config::{Config, expand_home_path};

pub(crate) fn config_from_env() -> Result<Config> {
    parse_args(env::args().skip(1))
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Config> {
    let args = args.collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help") {
        print_help();
        std::process::exit(0);
    }

    let mut config = Config::load()?;
    let mut args = args.into_iter().peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-d" | "--device" => {
                config.device = args
                    .next()
                    .ok_or_else(|| anyhow!("{arg} requires a device path"))?;
            }
            "-w" | "--width" => {
                config.width = parse_positive_arg(&arg, args.next())?;
            }
            "-h" | "--height" => {
                config.height = parse_positive_arg(&arg, args.next())?;
            }
            "-f" | "--fps" => {
                config.fps = parse_positive_arg(&arg, args.next())?;
            }
            "--camera-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("{arg} requires a directory path"))?;
                config.camera_dir = expand_home_path(&value);
            }
            "--force" => config.force = true,
            "--mirror-horizontal" => config.mirror_horizontal = true,
            "--no-audio" => config.audio = false,
            "--audio-input" => {
                config.audio_input = args
                    .next()
                    .ok_or_else(|| anyhow!("{arg} requires an input name"))?;
                config.audio = true;
            }
            _ => bail!("unknown argument: {arg}"),
        }
    }

    if config.width == 0 || config.height == 0 {
        bail!("width and height must be greater than zero");
    }
    if config.fps == 0 {
        bail!("fps must be greater than zero");
    }

    Ok(config)
}

fn parse_positive_arg(flag: &str, value: Option<String>) -> Result<u32> {
    let raw = value.ok_or_else(|| anyhow!("{flag} requires a value"))?;
    raw.parse::<u32>()
        .with_context(|| format!("{flag} expects a positive integer"))
}

fn print_help() {
    println!(
        "\
camilo - camera app for the terminal

Usage:
  camilo [--device /dev/video0] [--width 1920] [--height 1080] [--fps 30] [--camera-dir ~/Pictures/Camera] [--mirror-horizontal] [--audio-input default] [--no-audio] [--force]

Controls:
  Right-side shutter button  take pictures or start/stop video recording
  Right-side mode switch     toggle photo/video mode
  q                          exit
"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_accepts_values_above_camera_probe_cap() {
        let config = parse_args(["--fps".to_string(), "240".to_string()].into_iter())
            .expect("high preview fps should be accepted");

        assert_eq!(config.fps, 240);
    }

    #[test]
    fn fps_rejects_zero() {
        let error = parse_args(["--fps".to_string(), "0".to_string()].into_iter())
            .expect_err("zero fps should fail");

        assert!(error.to_string().contains("fps must be greater than zero"));
    }
}
