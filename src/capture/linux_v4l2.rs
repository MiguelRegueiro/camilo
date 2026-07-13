#![cfg(not(target_os = "freebsd"))]

use std::{
    io::{self, ErrorKind},
    sync::{Arc, Mutex},
    thread,
};

use crate::config::Config;

use super::{
    camera_stream::{
        LatestCameraFrame, V4l2FrameConfig, V4l2StreamInfo, camera_stream_should_stop,
        preferred_v4l2_formats, store_latest_frame,
    },
    rgb_frame::{
        V4l2PixelFormat, decode_camera_frame, frame_len, mirror_rgb24_in_place, v4l2_fourcc,
    },
};

#[cfg(not(target_os = "freebsd"))]
pub(super) fn run_capture(
    config: &Config,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    ready: &std::sync::mpsc::Sender<std::result::Result<V4l2StreamInfo, String>>,
) -> std::result::Result<(), String> {
    use v4l::buffer::Type;
    use v4l::io::mmap::Stream as MmapStream;
    use v4l::prelude::*;
    use v4l::video::Capture;
    use v4l::video::capture::Parameters;

    let device = Device::with_path(&config.device)
        .map_err(|error| format!("failed to open v4l2 device {}: {error}", config.device))?;

    let mut setup_errors = Vec::new();
    let Some((mut stream, frame_config, mut buffer)) = preferred_v4l2_formats(config)
        .into_iter()
        .find_map(|pixel_format| {
            let requested = v4l::Format::new(
                config.width,
                config.height,
                v4l::FourCC::new(&pixel_format.fourcc_bytes()),
            );
            let actual = match device.set_format(&requested) {
                Ok(actual) => actual,
                Err(error) => {
                    setup_errors.push(format!("{pixel_format:?}: {error}"));
                    return None;
                }
            };
            let Some(actual_format) = V4l2PixelFormat::from_fourcc(v4l2_fourcc(actual.fourcc.repr))
            else {
                setup_errors.push(format!(
                    "{pixel_format:?}: device selected unsupported {}",
                    actual.fourcc
                ));
                return None;
            };
            if let Err(error) = device.set_params(&Parameters::with_fps(config.fps)) {
                setup_errors.push(format!(
                    "{actual_format:?}: failed to set fps {}: {error}",
                    config.fps
                ));
                return None;
            }
            let frame_len = match frame_len(actual.width, actual.height) {
                Ok(frame_len) => frame_len,
                Err(error) => {
                    setup_errors.push(format!(
                        "{actual_format:?}: invalid frame size {}x{}: {error}",
                        actual.width, actual.height
                    ));
                    return None;
                }
            };
            let frame_config = V4l2FrameConfig {
                pixel_format: actual_format,
                width: actual.width,
                height: actual.height,
                mirror_horizontal: config.mirror_horizontal,
                frame_len,
            };
            let mut stream = match MmapStream::with_buffers(&device, Type::VideoCapture, 2) {
                Ok(stream) => stream,
                Err(error) => {
                    setup_errors.push(format!(
                        "{actual_format:?}: failed to create mmap stream: {error}"
                    ));
                    return None;
                }
            };
            stream.set_timeout(std::time::Duration::from_secs(3));
            let mut buffer = vec![0_u8; frame_config.frame_len];
            match store_v4l2_next_frame(&mut stream, frame_config, latest_frame, &mut buffer) {
                Ok(reused) => {
                    stream.set_timeout(std::time::Duration::from_millis(100));
                    Some((stream, frame_config, reused))
                }
                Err(error) => {
                    setup_errors.push(format!(
                        "{actual_format:?}: failed to read first frame: {error}"
                    ));
                    None
                }
            }
        })
    else {
        return Err(format!(
            "v4l2 device did not produce a frame for a supported RGB/YUYV/MJPG preview mode near {}x{}{}",
            config.width,
            config.height,
            if setup_errors.is_empty() {
                String::new()
            } else {
                format!(": {}", setup_errors.join("; "))
            }
        ));
    };
    let _ = ready.send(Ok(V4l2StreamInfo {
        width: frame_config.width,
        height: frame_config.height,
        input_format: frame_config.pixel_format.input_format(),
    }));

    loop {
        if camera_stream_should_stop(latest_frame) {
            break;
        }
        match store_v4l2_next_frame(&mut stream, frame_config, latest_frame, &mut buffer) {
            Ok(reused) => buffer = reused,
            Err(error) if error.kind() == ErrorKind::TimedOut => {}
            Err(error) if is_transient_v4l2_runtime_read_error(&error) => {
                thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(error) => return Err(format!("failed to read v4l2 frame: {error}")),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn is_transient_v4l2_runtime_read_error(error: &io::Error) -> bool {
    error.raw_os_error() == Some(22)
}

#[cfg(not(target_os = "freebsd"))]
fn store_v4l2_next_frame(
    stream: &mut v4l::io::mmap::Stream<'_>,
    config: V4l2FrameConfig,
    latest_frame: &Arc<Mutex<LatestCameraFrame>>,
    buffer: &mut [u8],
) -> io::Result<Vec<u8>> {
    use v4l::io::traits::CaptureStream;

    let (frame, metadata) = stream.next()?;
    let used = (metadata.bytesused as usize).min(frame.len());
    decode_camera_frame(
        config.pixel_format,
        &frame[..used],
        config.width,
        config.height,
        buffer,
    )
    .map_err(io::Error::other)?;
    if config.mirror_horizontal {
        mirror_rgb24_in_place(buffer, config.width, config.height);
    }
    Ok(store_latest_frame(
        latest_frame,
        buffer.to_vec(),
        config.frame_len,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4l2_format_preference_honors_probed_mjpeg_mode_first() {
        let config = Config {
            input_format: Some("mjpeg".to_string()),
            ..Config::default()
        };

        assert_eq!(
            preferred_v4l2_formats(&config),
            vec![
                V4l2PixelFormat::Mjpeg,
                V4l2PixelFormat::Yuyv,
                V4l2PixelFormat::Rgb24,
            ]
        );
    }

    #[test]
    fn v4l2_format_preference_tries_mjpeg_before_yuyv_without_probe() {
        let config = Config::default();

        assert_eq!(
            preferred_v4l2_formats(&config),
            vec![
                V4l2PixelFormat::Mjpeg,
                V4l2PixelFormat::Yuyv,
                V4l2PixelFormat::Rgb24
            ]
        );
    }
}
