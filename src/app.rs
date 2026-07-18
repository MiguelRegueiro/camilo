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
        latest_capture_thumbnail, latest_image_path, resize_rgb24, save_capture, square_thumbnail,
    },
    cli,
    config::PreviewBackend,
    terminal::{
        CaptureMode, InputTargets, KITTY_COUNTDOWN_IMAGE_ID, KITTY_IMAGE_IDS,
        KITTY_NO_MIC_IMAGE_ID, KITTY_PLACEMENT_ID, KITTY_THUMBNAIL_IMAGE_IDS,
        KITTY_THUMBNAIL_PLACEMENT_ID, KITTY_TIMER_IMAGE_ID, KittyFramePlacement, SelfTimer,
        TerminalGuard, clear_screen_and_images, drain_input_events, draw_sidebar,
        enable_tmux_passthrough, image_area_pixel_size, inside_tmux, looks_like_kitty,
        refresh_tmux_pane_origin, spawn_input_thread, ui_layout, write_kitty_countdown,
        write_kitty_delete_image, write_kitty_mode_pill, write_kitty_no_mic_pill,
        write_kitty_recording_timer, write_kitty_rgb_frame, write_kitty_self_timer_button,
        write_kitty_shutter_button,
    },
};

struct ActiveRecording {
    encoder: VideoRecording,
    started_at: Instant,
    next_frame_at: Instant,
    frame_interval: Duration,
    last_timer_second: Option<u64>,
}

struct PendingRecordingStop {
    handle: thread::JoinHandle<Result<()>>,
}

#[derive(Clone, Copy)]
enum PendingCaptureAction {
    Photo,
    StartRecording,
}

struct CountdownState {
    started_at: Instant,
    duration: Duration,
    action: PendingCaptureAction,
    last_visible_second: Option<u64>,
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

    let camera_frame_len = frame_len(config.width, config.height)?;
    let mut frame = vec![0_u8; camera_frame_len];

    let _terminal = TerminalGuard::enter()?;
    let stop_rx = spawn_input_thread();
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(camera_frame_len + camera_frame_len / 2, stdout.lock());
    let mut kitty_sequence = Vec::with_capacity(camera_frame_len + camera_frame_len / 2 + 4096);
    let mut layout = ui_layout(config.width, config.height);
    let mut layout_dirty = true;
    let mut preview_frame = Vec::new();
    let mut preview_width = config.width;
    let mut preview_height = config.height;
    let mut last_layout = None;
    let mut capture_mode = CaptureMode::Photo;
    let mut self_timer = SelfTimer::Off;
    let mut countdown: Option<CountdownState> = None;
    let mut countdown_visible = false;
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
    let mut pending_recording_stops = Vec::<PendingRecordingStop>::new();
    let mut no_mic_visible = false;
    let audio_available = audio_input_available(&config);
    let mut have_frame = false;

    loop {
        collect_finished_recordings(&mut pending_recording_stops)?;

        if drain_input_events(
            &stop_rx,
            last_layout,
            InputTargets {
                capture_mode: &mut capture_mode,
                self_timer: &mut self_timer,
                controls_locked: recording.is_some() || countdown.is_some(),
                capture_requested: &mut capture_requested,
                chrome_dirty: &mut chrome_dirty,
                layout_dirty: &mut layout_dirty,
            },
        ) {
            break;
        }

        let new_frame = match camera.read_latest_frame(&mut frame) {
            Ok(CameraFrameStatus::NewFrame) => {
                have_frame = true;
                true
            }
            Ok(CameraFrameStatus::NoFrame) => {
                thread::sleep(Duration::from_millis(1));
                if !have_frame {
                    continue;
                }
                false
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
        };

        if drain_input_events(
            &stop_rx,
            last_layout,
            InputTargets {
                capture_mode: &mut capture_mode,
                self_timer: &mut self_timer,
                controls_locked: recording.is_some() || countdown.is_some(),
                capture_requested: &mut capture_requested,
                chrome_dirty: &mut chrome_dirty,
                layout_dirty: &mut layout_dirty,
            },
        ) {
            break;
        }

        if let Some(recording) = recording.as_mut() {
            write_due_recording_frames(recording, &config, &frame, Instant::now())?;
        }

        if layout_dirty {
            if inside_tmux() {
                refresh_tmux_pane_origin();
            }
            let next_layout = ui_layout(config.width, config.height);
            if last_layout != Some(next_layout) {
                clear_screen_and_images(&mut out)?;
                draw_sidebar(&mut out, next_layout)?;
                previous_image_id = None;
                previous_thumbnail_image_id = None;
                frame_serial = 0;
                thumbnail_dirty = true;
                chrome_dirty = true;
                no_mic_visible = false;
                countdown_visible = false;
                if let Some(countdown) = countdown.as_mut() {
                    countdown.last_visible_second = None;
                }
            }
            layout = next_layout;
            last_layout = Some(layout);
            (preview_width, preview_height) = image_area_pixel_size(layout.preview_area);
            preview_width = preview_width.min(config.width).max(1);
            preview_height = preview_height.min(config.height).max(1);
            let preview_len = frame_len(preview_width, preview_height)?;
            if preview_frame.len() != preview_len {
                preview_frame.resize(preview_len, 0);
            }
            layout_dirty = false;
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
            if let Some(area) = layout.self_timer_area {
                write_kitty_self_timer_button(&mut out, area, self_timer, &mut kitty_sequence)?;
            }
            chrome_dirty = false;
        }

        if capture_requested {
            if countdown.is_some() {
                countdown = None;
                if countdown_visible {
                    write_kitty_delete_image(&mut out, KITTY_COUNTDOWN_IMAGE_ID)?;
                    countdown_visible = false;
                }
            } else {
                match capture_mode {
                    CaptureMode::Photo => {
                        if let Some(seconds) = self_timer.seconds() {
                            countdown = Some(CountdownState {
                                started_at: Instant::now(),
                                duration: Duration::from_secs(seconds),
                                action: PendingCaptureAction::Photo,
                                last_visible_second: None,
                            });
                        } else {
                            take_photo(
                                &config,
                                &frame,
                                &mut last_thumbnail,
                                &mut thumbnail_dirty,
                                &mut next_thumbnail_rescan,
                            )?;
                        }
                    }
                    CaptureMode::Video => {
                        if let Some(recording) = recording.take() {
                            pending_recording_stops.push(PendingRecordingStop {
                                handle: recording.encoder.stop_async(),
                            });
                            write_kitty_delete_image(&mut out, KITTY_TIMER_IMAGE_ID)?;
                            if no_mic_visible {
                                write_kitty_delete_image(&mut out, KITTY_NO_MIC_IMAGE_ID)?;
                                no_mic_visible = false;
                            }
                        } else if let Some(seconds) = self_timer.seconds() {
                            countdown = Some(CountdownState {
                                started_at: Instant::now(),
                                duration: Duration::from_secs(seconds),
                                action: PendingCaptureAction::StartRecording,
                                last_visible_second: None,
                            });
                        } else {
                            recording = Some(start_recording(&config, audio_available)?);
                        }
                        chrome_dirty = true;
                    }
                }
            }
            capture_requested = false;
        }

        if let Some(state) = countdown.as_mut() {
            let elapsed = state.started_at.elapsed();
            if elapsed >= state.duration {
                let action = state.action;
                countdown = None;
                if countdown_visible {
                    write_kitty_delete_image(&mut out, KITTY_COUNTDOWN_IMAGE_ID)?;
                    countdown_visible = false;
                }
                match action {
                    PendingCaptureAction::Photo => {
                        take_photo(
                            &config,
                            &frame,
                            &mut last_thumbnail,
                            &mut thumbnail_dirty,
                            &mut next_thumbnail_rescan,
                        )?;
                    }
                    PendingCaptureAction::StartRecording => {
                        recording = Some(start_recording(&config, audio_available)?);
                        chrome_dirty = true;
                    }
                }
            } else if let Some(area) = layout.countdown_area {
                let remaining = (state.duration - elapsed).as_secs_f64().ceil().max(1.0) as u64;
                if state.last_visible_second != Some(remaining) {
                    write_kitty_countdown(&mut out, area, remaining, &mut kitty_sequence)?;
                    state.last_visible_second = Some(remaining);
                    countdown_visible = true;
                }
            }
        } else if countdown_visible {
            write_kitty_delete_image(&mut out, KITTY_COUNTDOWN_IMAGE_ID)?;
            countdown_visible = false;
        }

        if let (Some(recording), Some(area)) = (&mut recording, layout.recording_timer_area) {
            let elapsed = recording.started_at.elapsed();
            let elapsed_second = elapsed.as_secs();
            if recording.last_timer_second != Some(elapsed_second) {
                write_kitty_recording_timer(&mut out, area, elapsed, &mut kitty_sequence)?;
                recording.last_timer_second = Some(elapsed_second);
            }
        }

        if !new_frame {
            out.flush()?;
            continue;
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
        let (display_frame, display_width, display_height) =
            if preview_width == config.width && preview_height == config.height {
                (frame.as_slice(), config.width, config.height)
            } else {
                resize_rgb24(
                    &frame,
                    config.width,
                    config.height,
                    &mut preview_frame,
                    preview_width,
                    preview_height,
                )?;
                (preview_frame.as_slice(), preview_width, preview_height)
            };
        write_kitty_rgb_frame(
            &mut out,
            KittyFramePlacement {
                image_id,
                placement_id: KITTY_PLACEMENT_ID,
                z_index: 0,
                previous_image_id,
                width: display_width,
                height: display_height,
                area: layout.preview_area,
            },
            display_frame,
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
    wait_for_recording_stops(pending_recording_stops)?;

    Ok(())
}

fn collect_finished_recordings(recordings: &mut Vec<PendingRecordingStop>) -> Result<()> {
    let mut index = 0;
    while index < recordings.len() {
        if recordings[index].handle.is_finished() {
            let recording = recordings.swap_remove(index);
            finish_recording_stop(recording)?;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn wait_for_recording_stops(recordings: Vec<PendingRecordingStop>) -> Result<()> {
    for recording in recordings {
        finish_recording_stop(recording)?;
    }
    Ok(())
}

fn finish_recording_stop(recording: PendingRecordingStop) -> Result<()> {
    recording
        .handle
        .join()
        .unwrap_or_else(|_| bail!("video recording finalizer panicked"))
}

fn take_photo(
    config: &crate::config::Config,
    frame: &[u8],
    last_thumbnail: &mut Option<CaptureThumbnail>,
    thumbnail_dirty: &mut bool,
    next_thumbnail_rescan: &mut Instant,
) -> Result<()> {
    let path = save_capture(config, frame)?;
    *last_thumbnail = Some(CaptureThumbnail {
        path,
        frame: square_thumbnail(frame, config.width, config.height, THUMBNAIL_SIZE),
    });
    *thumbnail_dirty = true;
    *next_thumbnail_rescan = Instant::now() + Duration::from_millis(750);
    Ok(())
}

fn start_recording(
    config: &crate::config::Config,
    audio_available: bool,
) -> Result<ActiveRecording> {
    let encoder = if audio_available {
        VideoRecording::start(config)?
    } else {
        VideoRecording::start_without_audio(config)?
    };
    let now = Instant::now();
    Ok(ActiveRecording {
        encoder,
        started_at: now,
        next_frame_at: now,
        frame_interval: recording_frame_interval(config.fps),
        last_timer_second: None,
    })
}

fn write_due_recording_frames(
    recording: &mut ActiveRecording,
    config: &crate::config::Config,
    frame: &[u8],
    now: Instant,
) -> Result<()> {
    while recording.next_frame_at <= now {
        match recording.encoder.write_frame(config, frame)? {
            RecordingWriteStatus::Written => {}
            RecordingWriteStatus::RestartedWithoutAudio => {
                recording.last_timer_second = None;
            }
        }
        recording.next_frame_at += recording.frame_interval;
    }
    Ok(())
}

fn recording_frame_interval(fps: u32) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(fps.max(1)))
}
