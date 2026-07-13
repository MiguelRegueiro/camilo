use anyhow::Result;
#[cfg(not(target_os = "freebsd"))]
use anyhow::{Context, anyhow};

use crate::config::Config;

#[cfg(target_os = "freebsd")]
use super::freebsd_v4l2;
#[cfg(not(target_os = "freebsd"))]
use super::rgb_frame::{V4l2PixelFormat, v4l2_fourcc};

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

pub(super) fn camera_mode_preference(mode: &CameraMode) -> (u64, u32, u8) {
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

pub(super) fn fps_from_interval(numerator: u32, denominator: u32) -> Option<u32> {
    (numerator > 0 && denominator > 0).then(|| {
        (f64::from(denominator) / f64::from(numerator))
            .round()
            .clamp(1.0, 120.0) as u32
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
