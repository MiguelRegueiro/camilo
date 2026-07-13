mod env;
mod input;
mod kitty_graphics;
mod layout;
mod raster;
mod session;
mod widgets;

pub(crate) use env::{enable_tmux_passthrough, inside_tmux, looks_like_kitty};
pub(crate) use input::{CaptureMode, drain_input_events, spawn_input_thread};
pub(crate) use kitty_graphics::{
    KITTY_IMAGE_IDS, KITTY_NO_MIC_IMAGE_ID, KITTY_PLACEMENT_ID, KITTY_THUMBNAIL_IMAGE_IDS,
    KITTY_THUMBNAIL_PLACEMENT_ID, KITTY_TIMER_IMAGE_ID, KittyFramePlacement,
    clear_screen_and_images, write_kitty_delete_image, write_kitty_rgb_frame,
};
pub(crate) use layout::{image_area_pixel_size, ui_layout};
pub(crate) use session::TerminalGuard;
pub(crate) use widgets::{
    draw_sidebar, write_kitty_mode_pill, write_kitty_no_mic_pill, write_kitty_recording_timer,
    write_kitty_shutter_button,
};
