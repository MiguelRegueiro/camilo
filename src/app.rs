use std::{
    io::{self, BufWriter, Write},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};

use crate::{
    capture::{
        CameraStream, CaptureThumbnail, THUMBNAIL_SIZE, frame_len, latest_capture_thumbnail,
        latest_image_path, save_capture, square_thumbnail,
    },
    cli,
    terminal::{
        KITTY_IMAGE_IDS, KITTY_PLACEMENT_ID, KITTY_THUMBNAIL_IMAGE_IDS,
        KITTY_THUMBNAIL_PLACEMENT_ID, KittyFramePlacement, TerminalGuard, clear_screen_and_images,
        drain_input_events, draw_sidebar, enable_tmux_passthrough, inside_tmux, looks_like_kitty,
        spawn_input_thread, ui_layout, write_kitty_delete_image, write_kitty_rgb_frame,
        write_kitty_shutter_button,
    },
};

pub(crate) fn run() -> Result<()> {
    let config = cli::config_from_env()?;

    if !config.force && !looks_like_kitty() {
        bail!(
            "this first version targets Kitty graphics; run from kitty or pass --force if your terminal is compatible"
        );
    }

    if inside_tmux() {
        enable_tmux_passthrough();
    }

    let mut camera = CameraStream::spawn(&config)?;
    let frame_len = frame_len(config.width, config.height)?;
    let mut frame = vec![0_u8; frame_len];

    let _terminal = TerminalGuard::enter()?;
    let stop_rx = spawn_input_thread();
    let stdout = io::stdout();
    let mut out = BufWriter::with_capacity(frame_len + frame_len / 2, stdout.lock());
    let mut kitty_sequence = Vec::with_capacity(frame_len + frame_len / 2 + 4096);
    let mut last_layout = None;
    let mut previous_image_id = None;
    let mut previous_thumbnail_image_id = None;
    let mut frame_serial = 0_u32;
    let mut thumbnail_serial = 0_u32;
    let mut last_thumbnail = latest_capture_thumbnail(&config.camera_dir, THUMBNAIL_SIZE);
    let mut thumbnail_dirty = last_thumbnail.is_some();
    let mut next_thumbnail_rescan = Instant::now() + Duration::from_millis(750);
    let mut capture_requested = false;

    loop {
        if drain_input_events(&stop_rx, last_layout, &mut capture_requested) {
            break;
        }

        match camera.read_frame(&mut frame) {
            Ok(true) => {}
            Ok(false) => {
                camera.stop();
                let stderr = camera.stderr_text();
                if stderr.trim().is_empty() {
                    bail!("camera stream ended before a full frame was received");
                }
                bail!("camera stream ended: {}", stderr.trim());
            }
            Err(error) => bail!("failed to read camera frame: {error}"),
        }

        if drain_input_events(&stop_rx, last_layout, &mut capture_requested) {
            break;
        }

        let layout = ui_layout(config.width, config.height);
        if last_layout != Some(layout) {
            clear_screen_and_images(&mut out)?;
            draw_sidebar(&mut out, layout)?;
            if let Some(area) = layout.shutter_area {
                write_kitty_shutter_button(&mut out, area, &mut kitty_sequence)?;
            }
            last_layout = Some(layout);
            previous_image_id = None;
            previous_thumbnail_image_id = None;
            frame_serial = 0;
            thumbnail_dirty = true;
        }

        if capture_requested {
            let path = save_capture(&config, &frame)?;
            last_thumbnail = Some(CaptureThumbnail {
                path,
                frame: square_thumbnail(&frame, config.width, config.height, THUMBNAIL_SIZE),
            });
            thumbnail_dirty = true;
            next_thumbnail_rescan = Instant::now() + Duration::from_millis(750);
            capture_requested = false;
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
        let z_index = (frame_serial % 1_000_000_000) as i32;
        write_kitty_rgb_frame(
            &mut out,
            KittyFramePlacement {
                image_id,
                placement_id: KITTY_PLACEMENT_ID,
                z_index,
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

    Ok(())
}
