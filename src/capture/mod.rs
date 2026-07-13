mod camera_modes;
mod camera_stream;
mod ffmpeg;
#[cfg(target_os = "freebsd")]
mod freebsd_v4l2;
mod latest_capture_thumbnail;
#[cfg(not(target_os = "freebsd"))]
mod linux_v4l2;
mod photo_capture;
mod rgb_frame;
mod video_recording;

pub(crate) use camera_modes::apply_best_camera_mode;
pub(crate) use camera_stream::{CameraFrameStatus, CameraStream};
pub(crate) use latest_capture_thumbnail::{
    CaptureThumbnail, THUMBNAIL_SIZE, latest_capture_thumbnail, latest_image_path, square_thumbnail,
};
pub(crate) use photo_capture::save_capture;
pub(crate) use rgb_frame::frame_len;
pub(crate) use video_recording::{RecordingWriteStatus, VideoRecording, audio_input_available};
