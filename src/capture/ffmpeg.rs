use std::{
    io::Read,
    process::ChildStderr,
    sync::{Arc, Mutex},
    thread,
};

use crate::config::Config;

fn add_input_format_args(args: &mut Vec<String>, config: &Config) {
    if let Some(input_format) = &config.input_format {
        args.push("-input_format".to_string());
        args.push(input_format.clone());
    }
}

pub(super) fn read_stderr_async(
    stderr_pipe: ChildStderr,
) -> (Arc<Mutex<String>>, thread::JoinHandle<()>) {
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

pub(super) fn camera_stream_ffmpeg_args(config: &Config) -> Vec<String> {
    let video_filter = ffmpeg_video_filter(config);
    let input_size = format!("{}x{}", config.width, config.height);
    let framerate = config.fps.to_string();
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-nostdin".to_string(),
        "-fflags".to_string(),
        "nobuffer".to_string(),
        "-flags".to_string(),
        "low_delay".to_string(),
        "-f".to_string(),
        "v4l2".to_string(),
    ];
    add_input_format_args(&mut args, config);
    args.extend([
        "-thread_queue_size".to_string(),
        "1".to_string(),
        "-framerate".to_string(),
        framerate,
        "-video_size".to_string(),
        input_size,
        "-i".to_string(),
        config.device.clone(),
        "-an".to_string(),
        "-sn".to_string(),
        "-dn".to_string(),
        "-vf".to_string(),
        video_filter,
        "-pix_fmt".to_string(),
        "rgb24".to_string(),
        "-f".to_string(),
        "rawvideo".to_string(),
        "pipe:1".to_string(),
    ]);
    args
}

pub(super) fn ffmpeg_video_filter(config: &Config) -> String {
    let mirror = if config.mirror_horizontal {
        "hflip,"
    } else {
        ""
    };
    format!(
        "{mirror}scale={}:{}:force_original_aspect_ratio=increase,crop={}:{}:(iw-ow)/2:(ih-oh)/2,format=rgb24",
        config.width, config.height, config.width, config.height
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_stream_args_cap_input_queue_before_device() {
        let config = Config::default();
        let args = camera_stream_ffmpeg_args(&config);
        let queue_index = args
            .iter()
            .position(|arg| arg == "-thread_queue_size")
            .expect("input queue should be capped");
        let input_index = args
            .iter()
            .position(|arg| arg == "-i")
            .expect("camera input should be present");

        assert_eq!(args.get(queue_index + 1).map(String::as_str), Some("1"));
        assert!(queue_index < input_index);
    }

    #[test]
    fn ffmpeg_filter_adds_hflip_when_horizontal_mirror_is_enabled() {
        let mut config = Config::default();

        assert!(ffmpeg_video_filter(&config).starts_with("hflip,"));

        config.mirror_horizontal = false;
        let filter = ffmpeg_video_filter(&config);
        assert!(filter.starts_with("scale="));
        assert!(filter.contains("force_original_aspect_ratio=increase,crop="));
        assert!(!filter.contains("fps="));
    }
}
