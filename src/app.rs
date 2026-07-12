use std::{
    io::{self, BufWriter, Write},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};

use crate::{
    capture::{
        CameraFrameStatus, CameraStream, CaptureThumbnail, RecordingWriteStatus, THUMBNAIL_SIZE,
        VideoRecording, apply_best_camera_mode, audio_input_available, frame_len,
        latest_capture_thumbnail, latest_image_path, save_capture, square_thumbnail,
    },
    cli,
    config::PreviewBackend,
    terminal::{
        CaptureMode, KITTY_IMAGE_IDS, KITTY_NO_MIC_IMAGE_ID, KITTY_PLACEMENT_ID,
        KITTY_THUMBNAIL_IMAGE_IDS, KITTY_THUMBNAIL_PLACEMENT_ID, KITTY_TIMER_IMAGE_ID,
        KittyFramePlacement, TerminalGuard, clear_screen_and_images, drain_input_events,
        draw_sidebar, enable_tmux_passthrough, inside_tmux, looks_like_kitty, spawn_input_thread,
        ui_layout, write_kitty_delete_image, write_kitty_mode_pill, write_kitty_no_mic_pill,
        write_kitty_recording_timer, write_kitty_rgb_frame, write_kitty_shutter_button,
    },
};

struct ActiveRecording {
    encoder: VideoRecording,
    started_at: Instant,
    last_timer_second: Option<u64>,
}

pub(crate) fn run() -> Result<()> {
    let mut config = cli::config_from_env()?;
    apply_best_camera_mode(&mut config);
    if config.camera_info && config.preview_backend == PreviewBackend::Auto {
        config.preview_backend = PreviewBackend::V4l2;
    }

    if !config.camera_info && !config.force && !looks_like_kitty() {
        bail!(
            "Camilo currently targets Kitty graphics; run from kitty or pass --force if your terminal is compatible"
        );
    }

    if inside_tmux() {
        enable_tmux_passthrough();
    }

    let mut camera = CameraStream::spawn(&mut config)?;
    if config.camera_info {
        let input_format = config.input_format.as_deref().unwrap_or("unknown");
        println!(
            "camera: {} {}x{} {}fps {}",
            config.device, config.width, config.height, config.fps, input_format
        );
        camera.stop();
        return Ok(());
    }

    let frame_len = frame_len(config.width, config.height)?;
    let mut frame = vec![0_u8; frame_len];

    let _terminal = TerminalGuard::enter()?;
    let stop_rx = spawn_input_thread();
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(frame_len + frame_len / 2, stdout.lock());
    let mut kitty_sequence = Vec::with_capacity(frame_len + frame_len / 2 + 4096);
    let mut last_layout = None;
    let mut capture_mode = CaptureMode::Photo;
    let mut chrome_dirty = true;
    let mut previous_image_id = None;
    let mut previous_thumbnail_image_id = None;
    let mut frame_serial = 0_u32;
    let mut thumbnail_serial = 0_u32;
    let mut last_thumbnail = latest_capture_thumbnail(&config.camera_dir, THUMBNAIL_SIZE);
    let mut thumbnail_dirty = last_thumbnail.is_some();
    let mut next_thumbnail_rescan = Instant::now() + Duration::from_millis(750);
    let mut capture_requested = false;
    let mut recording: Option<ActiveRecording> = None;
    let mut no_mic_visible = false;
    let audio_available = audio_input_available(&config);

    loop {
        if drain_input_events(
            &stop_rx,
            last_layout,
            &mut capture_mode,
            recording.is_some(),
            &mut capture_requested,
            &mut chrome_dirty,
        ) {
            break;
        }

        match camera.read_latest_frame(&mut frame) {
            Ok(CameraFrameStatus::NewFrame) => {}
            Ok(CameraFrameStatus::NoFrame) => {
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Ok(CameraFrameStatus::Ended) => {
                camera.stop();
                let stderr = camera.stderr_text();
                if stderr.trim().is_empty() {
                    bail!("camera stream ended before a full frame was received");
                }
                bail!("camera stream ended: {}", stderr.trim());
            }
            Err(error) => bail!("failed to read camera frame: {error}"),
        }

        if drain_input_events(
            &stop_rx,
            last_layout,
            &mut capture_mode,
            recording.is_some(),
            &mut capture_requested,
            &mut chrome_dirty,
        ) {
            break;
        }

        if let Some(recording) = recording.as_mut() {
            match recording.encoder.write_frame(&config, &frame)? {
                RecordingWriteStatus::Written => {}
                RecordingWriteStatus::RestartedWithoutAudio => {
                    recording.last_timer_second = None;
                }
            }
        }

        let layout = ui_layout(config.width, config.height);
        if last_layout != Some(layout) {
            clear_screen_and_images(&mut out)?;
            draw_sidebar(&mut out, layout)?;
            last_layout = Some(layout);
            previous_image_id = None;
            previous_thumbnail_image_id = None;
            frame_serial = 0;
            thumbnail_dirty = true;
            chrome_dirty = true;
            no_mic_visible = false;
        }

        if chrome_dirty {
            if let Some(area) = layout.shutter_area {
                write_kitty_shutter_button(
                    &mut out,
                    area,
                    capture_mode,
                    recording.is_some(),
                    &mut kitty_sequence,
                )?;
            }
            if let Some(area) = layout.mode_pill_area {
                write_kitty_mode_pill(&mut out, area, capture_mode, &mut kitty_sequence)?;
            }
            chrome_dirty = false;
        }

        if capture_requested {
            match capture_mode {
                CaptureMode::Photo => {
                    let path = save_capture(&config, &frame)?;
                    last_thumbnail = Some(CaptureThumbnail {
                        path,
                        frame: square_thumbnail(
                            &frame,
                            config.width,
                            config.height,
                            THUMBNAIL_SIZE,
                        ),
                    });
                    thumbnail_dirty = true;
                    next_thumbnail_rescan = Instant::now() + Duration::from_millis(750);
                }
                CaptureMode::Video => {
                    if let Some(recording) = recording.take() {
                        recording.encoder.stop()?;
                        write_kitty_delete_image(&mut out, KITTY_TIMER_IMAGE_ID)?;
                        if no_mic_visible {
                            write_kitty_delete_image(&mut out, KITTY_NO_MIC_IMAGE_ID)?;
                            no_mic_visible = false;
                        }
                    } else {
                        let encoder = if audio_available {
                            VideoRecording::start(&config)?
                        } else {
                            VideoRecording::start_without_audio(&config)?
                        };
                        recording = Some(ActiveRecording {
                            encoder,
                            started_at: Instant::now(),
                            last_timer_second: None,
                        });
                    }
                    chrome_dirty = true;
                }
            }
            capture_requested = false;
        }

        if let (Some(recording), Some(area)) = (&mut recording, layout.recording_timer_area) {
            let elapsed = recording.started_at.elapsed();
            let elapsed_second = elapsed.as_secs();
            if recording.last_timer_second != Some(elapsed_second) {
                write_kitty_recording_timer(&mut out, area, elapsed, &mut kitty_sequence)?;
                recording.last_timer_second = Some(elapsed_second);
            }
        }

        if Instant::now() >= next_thumbnail_rescan {
            let current_path = last_thumbnail
                .as_ref()
                .map(|thumbnail| thumbnail.path.as_path());
            if latest_image_path(&config.camera_dir).as_deref() != current_path {
                last_thumbnail = latest_capture_thumbnail(&config.camera_dir, THUMBNAIL_SIZE);
                thumbnail_dirty = true;
            }
            next_thumbnail_rescan = Instant::now() + Duration::from_millis(750);
        }

        let image_id = KITTY_IMAGE_IDS[(frame_serial as usize) % KITTY_IMAGE_IDS.len()];
        write_kitty_rgb_frame(
            &mut out,
            KittyFramePlacement {
                image_id,
                placement_id: KITTY_PLACEMENT_ID,
                z_index: 0,
                previous_image_id,
                width: config.width,
                height: config.height,
                area: layout.preview_area,
            },
            &frame,
            &mut kitty_sequence,
        )?;
        previous_image_id = Some(image_id);
        frame_serial = frame_serial.wrapping_add(1);

        if let Some(area) = layout.no_mic_area {
            let no_audio = match &recording {
                Some(recording) => !recording.encoder.audio(),
                None => capture_mode == CaptureMode::Video && !audio_available,
            };
            if no_audio {
                write_kitty_no_mic_pill(&mut out, area, &mut kitty_sequence)?;
                no_mic_visible = true;
            } else if no_mic_visible {
                write_kitty_delete_image(&mut out, KITTY_NO_MIC_IMAGE_ID)?;
                no_mic_visible = false;
            }
        } else if no_mic_visible {
            write_kitty_delete_image(&mut out, KITTY_NO_MIC_IMAGE_ID)?;
            no_mic_visible = false;
        }

        if thumbnail_dirty {
            if let (Some(thumbnail), Some(area)) = (&last_thumbnail, layout.thumbnail_area) {
                let image_id = KITTY_THUMBNAIL_IMAGE_IDS
                    [(thumbnail_serial as usize) % KITTY_THUMBNAIL_IMAGE_IDS.len()];
                write_kitty_rgb_frame(
                    &mut out,
                    KittyFramePlacement {
                        image_id,
                        placement_id: KITTY_THUMBNAIL_PLACEMENT_ID,
                        z_index: 1,
                        previous_image_id: previous_thumbnail_image_id,
                        width: THUMBNAIL_SIZE,
                        height: THUMBNAIL_SIZE,
                        area,
                    },
                    &thumbnail.frame,
                    &mut kitty_sequence,
                )?;
                previous_thumbnail_image_id = Some(image_id);
                thumbnail_serial = thumbnail_serial.wrapping_add(1);
            } else if let Some(image_id) = previous_thumbnail_image_id.take() {
                write_kitty_delete_image(&mut out, image_id)?;
            }
            thumbnail_dirty = false;
        }

        out.flush()?;
    }

    if let Some(recording) = recording.take() {
        recording.encoder.stop()?;
    }
    camera.stop();

    Ok(())
}
