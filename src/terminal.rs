use std::{
    env,
    ffi::OsStr,
    io::{self, Write},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
};

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

const KITTY_IMAGE_ID: u32 = 0x4c_55_4d; // "LUM", within the 24-bit foreground-color-safe range.
pub(crate) const KITTY_IMAGE_IDS: [u32; 2] = [KITTY_IMAGE_ID, KITTY_IMAGE_ID + 1];
pub(crate) const KITTY_THUMBNAIL_IMAGE_IDS: [u32; 2] = [KITTY_IMAGE_ID + 10, KITTY_IMAGE_ID + 11];
pub(crate) const KITTY_SHUTTER_IMAGE_ID: u32 = KITTY_IMAGE_ID + 20;
pub(crate) const KITTY_MODE_IMAGE_ID: u32 = KITTY_IMAGE_ID + 30;
pub(crate) const KITTY_PLACEMENT_ID: u32 = 1;
pub(crate) const KITTY_THUMBNAIL_PLACEMENT_ID: u32 = 2;
pub(crate) const KITTY_SHUTTER_PLACEMENT_ID: u32 = 3;
pub(crate) const KITTY_MODE_PLACEMENT_ID: u32 = 4;
const KITTY_RAW_CHUNK_BYTES: usize = 3 * 4096 / 4;
const SIDEBAR_COLS: u16 = 16;
const MIN_PREVIEW_COLS: u16 = 20;
const MIN_SIDEBAR_COLS: u16 = 12;
const SHUTTER_SIZE: u32 = 128;
const MODE_PILL_WIDTH: u32 = 128;
const MODE_PILL_HEIGHT: u32 = 160;
const SHUTTER_Z_INDEX: i32 = 1_000_000_001;
const MODE_PILL_Z_INDEX: i32 = 1_000_000_002;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Rect {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

impl Rect {
    fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x
            && x < self.x.saturating_add(self.cols)
            && y >= self.y
            && y < self.y.saturating_add(self.rows)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageArea {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UiLayout {
    pub(crate) preview_area: ImageArea,
    pub(crate) sidebar: Rect,
    pub(crate) capture_button: Option<Rect>,
    pub(crate) shutter_area: Option<ImageArea>,
    pub(crate) mode_toggle: Option<Rect>,
    pub(crate) mode_pill_area: Option<ImageArea>,
    pub(crate) thumbnail_area: Option<ImageArea>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct KittyFramePlacement {
    pub(crate) image_id: u32,
    pub(crate) placement_id: u32,
    pub(crate) z_index: i32,
    pub(crate) previous_image_id: Option<u32>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) area: ImageArea,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CaptureMode {
    Photo,
    Video,
}

impl CaptureMode {
    pub(crate) fn toggle(&mut self) {
        *self = match self {
            Self::Photo => Self::Video,
            Self::Video => Self::Photo,
        };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputEvent {
    Quit,
    Capture,
    ToggleMode,
    Click { x: u16, y: u16 },
}

pub(crate) fn looks_like_kitty() -> bool {
    env::var("TERM")
        .map(|term| term.to_ascii_lowercase().contains("kitty"))
        .unwrap_or(false)
        || env::var_os("KITTY_WINDOW_ID").is_some()
        || env::var("TERM_PROGRAM")
            .map(|term| term.eq_ignore_ascii_case("kitty"))
            .unwrap_or(false)
}

pub(crate) struct TerminalGuard;

impl TerminalGuard {
    pub(crate) fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw terminal mode")?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            Clear(ClearType::All),
            Hide,
            EnableMouseCapture
        )
        .context("failed to enter terminal preview mode")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = write_kitty_apc_bytes(&mut stdout, clear_images_sequence().as_bytes());
        let _ = execute!(stdout, DisableMouseCapture, Show, LeaveAlternateScreen);
        let _ = stdout.flush();
        let _ = disable_raw_mode();
    }
}

pub(crate) fn spawn_input_thread() -> mpsc::Receiver<InputEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(Event::Key(key))
                    if key.code == KeyCode::Esc
                        || key.code == KeyCode::Char('q')
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)) =>
                {
                    let _ = tx.send(InputEvent::Quit);
                    break;
                }
                Ok(Event::Key(key))
                    if key.code == KeyCode::Char(' ') || key.code == KeyCode::Enter =>
                {
                    if tx.send(InputEvent::Capture).is_err() {
                        break;
                    }
                }
                Ok(Event::Key(key)) if key.code == KeyCode::Char('v') => {
                    if tx.send(InputEvent::ToggleMode).is_err() {
                        break;
                    }
                }
                Ok(Event::Mouse(mouse))
                    if mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
                {
                    if tx
                        .send(InputEvent::Click {
                            x: mouse.column,
                            y: mouse.row,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = tx.send(InputEvent::Quit);
                    break;
                }
            }
        }
    });
    rx
}

pub(crate) fn drain_input_events(
    rx: &mpsc::Receiver<InputEvent>,
    layout: Option<UiLayout>,
    capture_mode: &mut CaptureMode,
    capture_requested: &mut bool,
    chrome_dirty: &mut bool,
) -> bool {
    let mut should_quit = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            InputEvent::Quit => should_quit = true,
            InputEvent::Capture => *capture_requested = true,
            InputEvent::ToggleMode => {
                capture_mode.toggle();
                *chrome_dirty = true;
            }
            InputEvent::Click { x, y } => {
                if layout
                    .and_then(|layout| layout.capture_button)
                    .is_some_and(|button| button.contains(x, y))
                {
                    *capture_requested = true;
                } else if layout
                    .and_then(|layout| layout.mode_toggle)
                    .is_some_and(|toggle| toggle.contains(x, y))
                {
                    capture_mode.toggle();
                    *chrome_dirty = true;
                }
            }
        }
    }
    should_quit
}

pub(crate) fn ui_layout(source_width: u32, source_height: u32) -> UiLayout {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let (pixel_width, pixel_height) = terminal_pixel_size(cols, rows);
    let cell_width = f64::from(pixel_width) / f64::from(cols.max(1));
    let cell_height = f64::from(pixel_height) / f64::from(rows.max(1));
    let sidebar_cols = if cols >= MIN_PREVIEW_COLS + MIN_SIDEBAR_COLS {
        SIDEBAR_COLS.min(cols.saturating_sub(MIN_PREVIEW_COLS))
    } else {
        0
    };
    let preview_cols = cols.saturating_sub(sidebar_cols).max(1);
    let sidebar = Rect {
        x: preview_cols,
        y: 0,
        cols: sidebar_cols,
        rows,
    };
    let preview_bounds = Rect {
        x: 0,
        y: 0,
        cols: preview_cols,
        rows,
    };

    let shutter_slot = (sidebar_cols > 0).then(|| Rect {
        x: sidebar.x,
        y: rows.saturating_sub(5) / 2,
        cols: sidebar.cols,
        rows: 5.min(rows.max(1)),
    });
    let shutter_area = shutter_slot.and_then(|slot| shutter_area(slot, cell_width, cell_height));
    let capture_button = shutter_area.map(capture_hitbox);
    let (mode_toggle, mode_pill_area) = mode_pill_area(sidebar);
    let thumbnail_area = thumbnail_area(sidebar, rows, cell_width, cell_height);

    UiLayout {
        preview_area: fit_image_area(
            source_width,
            source_height,
            preview_bounds,
            cell_width,
            cell_height,
        ),
        sidebar,
        capture_button,
        shutter_area,
        mode_toggle,
        mode_pill_area,
        thumbnail_area,
    }
}

fn fit_image_area(
    source_width: u32,
    source_height: u32,
    bounds: Rect,
    cell_width: f64,
    cell_height: f64,
) -> ImageArea {
    let cols = bounds.cols.max(1);
    let rows = bounds.rows.max(1);
    let max_width_px = f64::from(cols) * cell_width;
    let max_height_px = f64::from(rows) * cell_height;
    let source_aspect = f64::from(source_width) / f64::from(source_height.max(1));

    let (display_width_px, display_height_px) = if max_width_px / max_height_px > source_aspect {
        (max_height_px * source_aspect, max_height_px)
    } else {
        (max_width_px, max_width_px / source_aspect)
    };

    let display_cols = ((display_width_px / cell_width).floor() as u16).clamp(1, cols.max(1));
    let display_rows = ((display_height_px / cell_height).floor() as u16).clamp(1, rows.max(1));

    ImageArea {
        x: bounds.x + cols.saturating_sub(display_cols) / 2,
        y: bounds.y + rows.saturating_sub(display_rows) / 2,
        cols: display_cols,
        rows: display_rows,
    }
}

fn mode_pill_area(sidebar: Rect) -> (Option<Rect>, Option<ImageArea>) {
    if sidebar.cols < 6 || sidebar.rows < 10 {
        return (None, None);
    }

    let cols = 12.min(sidebar.cols);
    let rect = Rect {
        x: sidebar.x + sidebar.cols.saturating_sub(cols) / 2,
        y: sidebar.y + 1,
        cols,
        rows: 8,
    };
    let area = ImageArea {
        x: rect.x,
        y: rect.y,
        cols: rect.cols,
        rows: rect.rows,
    };

    (Some(rect), Some(area))
}

fn thumbnail_area(
    sidebar: Rect,
    terminal_rows: u16,
    cell_width: f64,
    cell_height: f64,
) -> Option<ImageArea> {
    if sidebar.cols < 4 || terminal_rows < 4 {
        return None;
    }

    let thumb_cols = sidebar.cols.saturating_sub(2).max(1);
    let thumb_rows = ((f64::from(thumb_cols) * cell_width / cell_height).round() as u16)
        .clamp(1, terminal_rows.saturating_sub(2).max(1));

    Some(ImageArea {
        x: sidebar.x + 1,
        y: terminal_rows.saturating_sub(thumb_rows + 1),
        cols: thumb_cols,
        rows: thumb_rows,
    })
}

fn shutter_area(button: Rect, cell_width: f64, cell_height: f64) -> Option<ImageArea> {
    if button.cols == 0 || button.rows == 0 {
        return None;
    }

    let max_width_px = f64::from(button.cols) * cell_width;
    let max_height_px = f64::from(button.rows) * cell_height;
    let size_px = max_width_px.min(max_height_px) * 0.82;
    let cols = ((size_px / cell_width).round() as u16).clamp(1, button.cols);
    let rows = ((size_px / cell_height).round() as u16).clamp(1, button.rows);

    Some(ImageArea {
        x: button.x + button.cols.saturating_sub(cols) / 2,
        y: button.y + button.rows.saturating_sub(rows) / 2,
        cols,
        rows,
    })
}

fn capture_hitbox(area: ImageArea) -> Rect {
    Rect {
        x: area.x,
        y: area.y,
        cols: area.cols,
        rows: area.rows,
    }
}

fn terminal_pixel_size(cols: u16, rows: u16) -> (u32, u32) {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0;
    if ok {
        let size = unsafe { size.assume_init() };
        if size.ws_xpixel > 0 && size.ws_ypixel > 0 {
            return (u32::from(size.ws_xpixel), u32::from(size.ws_ypixel));
        }
    }

    // Conservative fallback for terminal emulators that do not expose pixel size.
    (u32::from(cols.max(1)) * 8, u32::from(rows.max(1)) * 16)
}

pub(crate) fn clear_screen_and_images(out: &mut impl Write) -> io::Result<()> {
    write_kitty_apc_bytes(out, clear_images_sequence().as_bytes())?;
    out.write_all(b"\x1b[2J\x1b[H")
}

pub(crate) fn draw_sidebar(out: &mut impl Write, layout: UiLayout) -> io::Result<()> {
    if layout.sidebar.cols == 0 {
        return Ok(());
    }

    out.write_all(b"\x1b[0m")
}

fn clear_images_sequence() -> &'static str {
    "\x1b_Ga=d,d=A,q=2\x1b\\"
}

pub(crate) fn write_kitty_delete_image(out: &mut impl Write, image_id: u32) -> io::Result<()> {
    write_kitty_apc_bytes(
        out,
        format!("\x1b_Ga=d,d=I,q=2,i={image_id}\x1b\\").as_bytes(),
    )
}

pub(crate) fn write_kitty_rgb_frame(
    out: &mut impl Write,
    placement: KittyFramePlacement,
    frame: &[u8],
    sequence: &mut Vec<u8>,
) -> io::Result<()> {
    write_kitty_image(out, placement, frame, 24, sequence)
}

pub(crate) fn write_kitty_shutter_button(
    out: &mut impl Write,
    area: ImageArea,
    capture_mode: CaptureMode,
    sequence: &mut Vec<u8>,
) -> io::Result<()> {
    let frame = shutter_button_rgba(SHUTTER_SIZE, capture_mode);
    write_kitty_image(
        out,
        KittyFramePlacement {
            image_id: KITTY_SHUTTER_IMAGE_ID,
            placement_id: KITTY_SHUTTER_PLACEMENT_ID,
            z_index: SHUTTER_Z_INDEX,
            previous_image_id: None,
            width: SHUTTER_SIZE,
            height: SHUTTER_SIZE,
            area,
        },
        &frame,
        32,
        sequence,
    )
}

fn write_kitty_image(
    out: &mut impl Write,
    placement: KittyFramePlacement,
    frame: &[u8],
    pixel_format: u8,
    sequence: &mut Vec<u8>,
) -> io::Result<()> {
    sequence.clear();
    write!(
        sequence,
        "\x1b[{};{}H",
        placement.area.y.saturating_add(1),
        placement.area.x.saturating_add(1)
    )?;

    let mut offset = 0;
    let mut first = true;
    let mut encoded = [0_u8; 4096];
    while offset < frame.len() {
        let end = (offset + KITTY_RAW_CHUNK_BYTES).min(frame.len());
        let more = end < frame.len();
        let encoded_len = BASE64
            .encode_slice(&frame[offset..end], &mut encoded)
            .map_err(io::Error::other)?;
        if first {
            write!(
                sequence,
                "\x1b_Ga=T,q=2,f={},s={},v={},i={},p={},c={},r={},C=1,z={},m={};",
                pixel_format,
                placement.width,
                placement.height,
                placement.image_id,
                placement.placement_id,
                placement.area.cols,
                placement.area.rows,
                placement.z_index,
                if more { 1 } else { 0 },
            )?;
            sequence.extend_from_slice(&encoded[..encoded_len]);
            sequence.extend_from_slice(b"\x1b\\");
            first = false;
        } else {
            write!(sequence, "\x1b_Gm={};", if more { 1 } else { 0 },)?;
            sequence.extend_from_slice(&encoded[..encoded_len]);
            sequence.extend_from_slice(b"\x1b\\");
        }
        offset = end;
    }
    if let Some(previous_image_id) = placement.previous_image_id
        && previous_image_id != placement.image_id
    {
        write!(sequence, "\x1b_Ga=d,d=I,q=2,i={previous_image_id}\x1b\\")?;
    }

    write_kitty_apc_bytes(out, sequence)
}

pub(crate) fn write_kitty_mode_pill(
    out: &mut impl Write,
    area: ImageArea,
    capture_mode: CaptureMode,
    sequence: &mut Vec<u8>,
) -> io::Result<()> {
    let frame = mode_pill_rgba(MODE_PILL_WIDTH, MODE_PILL_HEIGHT, capture_mode);
    write_kitty_image(
        out,
        KittyFramePlacement {
            image_id: KITTY_MODE_IMAGE_ID,
            placement_id: KITTY_MODE_PLACEMENT_ID,
            z_index: MODE_PILL_Z_INDEX,
            previous_image_id: None,
            width: MODE_PILL_WIDTH,
            height: MODE_PILL_HEIGHT,
            area,
        },
        &frame,
        32,
        sequence,
    )
}

fn shutter_button_rgba(size: u32, capture_mode: CaptureMode) -> Vec<u8> {
    let mut frame = vec![0_u8; (size * size * 4) as usize];
    let center = (f64::from(size) - 1.0) / 2.0;
    let outer_radius = f64::from(size) * 0.46;
    let inner_radius = f64::from(size) * 0.34;
    let gap_radius = f64::from(size) * 0.39;
    let inner_color = match capture_mode {
        CaptureMode::Photo => [250, 250, 251],
        CaptureMode::Video => [239, 68, 68],
    };

    for y in 0..size {
        for x in 0..size {
            let dx = f64::from(x) - center;
            let dy = f64::from(y) - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let offset = rgba_offset(size, x, y);

            let color = if distance <= inner_radius {
                Some((inner_color, edge_alpha(distance, inner_radius)))
            } else if distance >= gap_radius && distance <= outer_radius {
                let edge = edge_alpha(distance, outer_radius);
                let inner_edge = edge_alpha(gap_radius, distance);
                Some(([244, 244, 245], edge.min(inner_edge)))
            } else {
                None
            };

            if let Some((color, alpha)) = color {
                put_pixel(&mut frame, offset, color, alpha);
            }
        }
    }

    frame
}

fn mode_pill_rgba(width: u32, height: u32, capture_mode: CaptureMode) -> Vec<u8> {
    let mut frame = vec![0_u8; (width * height * 4) as usize];
    let outer = RoundedRect {
        x: 8.0,
        y: 5.0,
        width: f64::from(width - 16),
        height: f64::from(height - 10),
        radius: 30.0,
    };
    let top_item = RoundedRect {
        x: 20.0,
        y: 19.0,
        width: f64::from(width - 40),
        height: 58.0,
        radius: 23.0,
    };
    let bottom_item = RoundedRect {
        x: 20.0,
        y: f64::from(height) - 77.0,
        width: f64::from(width - 40),
        height: 58.0,
        radius: 23.0,
    };

    fill_rounded_rect(&mut frame, width, height, outer, [24, 24, 27], 138);
    let selected = match capture_mode {
        CaptureMode::Photo => top_item,
        CaptureMode::Video => bottom_item,
    };
    fill_rounded_rect(&mut frame, width, height, selected, [63, 63, 70], 205);

    let photo_color = if capture_mode == CaptureMode::Photo {
        [250, 250, 250]
    } else {
        [161, 161, 170]
    };
    let video_color = if capture_mode == CaptureMode::Video {
        [250, 250, 250]
    } else {
        [161, 161, 170]
    };
    draw_photo_glyph(&mut frame, width, height, 48.0, photo_color);
    draw_video_glyph(&mut frame, width, height, 112.0, video_color);

    frame
}

fn draw_photo_glyph(frame: &mut [u8], width: u32, height: u32, center_y: f64, color: [u8; 3]) {
    let center_x = f64::from(width) / 2.0;
    fill_rounded_rect(
        frame,
        width,
        height,
        RoundedRect {
            x: center_x - 25.0,
            y: center_y - 10.0,
            width: 50.0,
            height: 28.0,
            radius: 8.0,
        },
        color,
        238,
    );
    fill_rounded_rect(
        frame,
        width,
        height,
        RoundedRect {
            x: center_x - 9.0,
            y: center_y - 17.0,
            width: 18.0,
            height: 9.0,
            radius: 4.0,
        },
        color,
        238,
    );
    fill_circle(
        frame,
        width,
        height,
        Circle {
            x: center_x,
            y: center_y + 3.0,
            radius: 10.5,
        },
        [24, 24, 27],
        225,
    );
    fill_circle(
        frame,
        width,
        height,
        Circle {
            x: center_x,
            y: center_y + 3.0,
            radius: 4.5,
        },
        color,
        238,
    );
}

fn draw_video_glyph(frame: &mut [u8], width: u32, height: u32, center_y: f64, color: [u8; 3]) {
    let center_x = f64::from(width) / 2.0;
    let body_right = center_x + 13.0;
    fill_rounded_rect(
        frame,
        width,
        height,
        RoundedRect {
            x: center_x - 25.0,
            y: center_y - 11.0,
            width: 38.0,
            height: 22.0,
            radius: 6.0,
        },
        color,
        238,
    );
    fill_quad(
        frame,
        width,
        height,
        [
            (body_right - 1.0, center_y - 6.0),
            (center_x + 23.0, center_y - 12.0),
            (center_x + 23.0, center_y + 12.0),
            (body_right - 1.0, center_y + 6.0),
        ],
        color,
        238,
    );
}
#[derive(Clone, Copy)]
struct Circle {
    x: f64,
    y: f64,
    radius: f64,
}

fn fill_circle(
    frame: &mut [u8],
    width: u32,
    height: u32,
    circle: Circle,
    color: [u8; 3],
    alpha: u8,
) {
    for y in 0..height {
        for x in 0..width {
            let dx = f64::from(x) + 0.5 - circle.x;
            let dy = f64::from(y) + 0.5 - circle.y;
            let coverage = (circle.radius - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
            if coverage > 0.0 {
                let offset = rgba_offset(width, x, y);
                blend_pixel(
                    frame,
                    offset,
                    color,
                    (coverage * f64::from(alpha)).round() as u8,
                );
            }
        }
    }
}

fn fill_quad(
    frame: &mut [u8],
    width: u32,
    height: u32,
    points: [(f64, f64); 4],
    color: [u8; 3],
    alpha: u8,
) {
    fill_triangle(
        frame,
        width,
        height,
        [points[0], points[1], points[2]],
        color,
        alpha,
    );
    fill_triangle(
        frame,
        width,
        height,
        [points[0], points[2], points[3]],
        color,
        alpha,
    );
}

fn fill_triangle(
    frame: &mut [u8],
    width: u32,
    height: u32,
    points: [(f64, f64); 3],
    color: [u8; 3],
    alpha: u8,
) {
    let min_x = points
        .iter()
        .map(|(x, _)| *x)
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0) as u32;
    let max_x = points
        .iter()
        .map(|(x, _)| *x)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .min(f64::from(width - 1)) as u32;
    let min_y = points
        .iter()
        .map(|(_, y)| *y)
        .fold(f64::INFINITY, f64::min)
        .floor()
        .max(0.0) as u32;
    let max_y = points
        .iter()
        .map(|(_, y)| *y)
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil()
        .min(f64::from(height - 1)) as u32;

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            if point_in_triangle(f64::from(x) + 0.5, f64::from(y) + 0.5, points) {
                let offset = rgba_offset(width, x, y);
                blend_pixel(frame, offset, color, alpha);
            }
        }
    }
}

fn point_in_triangle(x: f64, y: f64, points: [(f64, f64); 3]) -> bool {
    let [(x1, y1), (x2, y2), (x3, y3)] = points;
    let d1 = (x - x2) * (y1 - y2) - (x1 - x2) * (y - y2);
    let d2 = (x - x3) * (y2 - y3) - (x2 - x3) * (y - y3);
    let d3 = (x - x1) * (y3 - y1) - (x3 - x1) * (y - y1);
    let has_negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_negative && has_positive)
}

#[derive(Clone, Copy)]
struct RoundedRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    radius: f64,
}

fn fill_rounded_rect(
    frame: &mut [u8],
    width: u32,
    height: u32,
    rect: RoundedRect,
    color: [u8; 3],
    alpha: u8,
) {
    for y in 0..height {
        for x in 0..width {
            let coverage = rounded_rect_coverage(f64::from(x) + 0.5, f64::from(y) + 0.5, rect);
            if coverage > 0.0 {
                let offset = rgba_offset(width, x, y);
                blend_pixel(
                    frame,
                    offset,
                    color,
                    (coverage * f64::from(alpha)).round() as u8,
                );
            }
        }
    }
}

fn rounded_rect_coverage(x: f64, y: f64, rect: RoundedRect) -> f64 {
    let half_width = rect.width / 2.0;
    let half_height = rect.height / 2.0;
    let center_x = rect.x + half_width;
    let center_y = rect.y + half_height;
    let qx = (x - center_x).abs() - (half_width - rect.radius);
    let qy = (y - center_y).abs() - (half_height - rect.radius);
    let outside_x = qx.max(0.0);
    let outside_y = qy.max(0.0);
    let distance =
        (outside_x * outside_x + outside_y * outside_y).sqrt() + qx.max(qy).min(0.0) - rect.radius;
    (0.5 - distance).clamp(0.0, 1.0)
}

fn put_pixel(frame: &mut [u8], offset: usize, color: [u8; 3], alpha: u8) {
    frame[offset] = color[0];
    frame[offset + 1] = color[1];
    frame[offset + 2] = color[2];
    frame[offset + 3] = alpha;
}

fn blend_pixel(frame: &mut [u8], offset: usize, color: [u8; 3], alpha: u8) {
    let source_alpha = f64::from(alpha) / 255.0;
    let dest_alpha = f64::from(frame[offset + 3]) / 255.0;
    let out_alpha = source_alpha + dest_alpha * (1.0 - source_alpha);
    if out_alpha <= f64::EPSILON {
        return;
    }

    for channel in 0..3 {
        let source = f64::from(color[channel]) * source_alpha;
        let dest = f64::from(frame[offset + channel]) * dest_alpha * (1.0 - source_alpha);
        frame[offset + channel] = ((source + dest) / out_alpha).round() as u8;
    }
    frame[offset + 3] = (out_alpha * 255.0).round() as u8;
}

fn rgba_offset(width: u32, x: u32, y: u32) -> usize {
    ((y * width + x) * 4) as usize
}

fn edge_alpha(distance: f64, radius: f64) -> u8 {
    ((radius - distance).clamp(0.0, 1.0) * 255.0).round() as u8
}

fn write_kitty_apc_bytes(out: &mut impl Write, sequence: &[u8]) -> io::Result<()> {
    if inside_tmux() {
        out.write_all(&wrap_kitty_apcs_for_tmux(sequence))
    } else {
        out.write_all(sequence)
    }
}

pub(crate) fn inside_tmux() -> bool {
    env::var_os("TMUX").is_some()
}

pub(crate) fn enable_tmux_passthrough() {
    let mut args = vec!["set-option".into(), "-p".into(), "-q".into()];
    if let Some(pane) = env::var_os("TMUX_PANE")
        && !pane.is_empty()
    {
        args.push("-t".into());
        args.push(pane);
    }
    args.push("allow-passthrough".into());
    args.push("on".into());

    let _ = Command::new("tmux")
        .args(args.iter().map(OsStr::new))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn wrap_kitty_apcs_for_tmux(sequence: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(sequence.len() + sequence.len() / 4);
    let mut i = 0;
    while i < sequence.len() {
        if sequence.len() - i >= 3
            && &sequence[i..i + 3] == b"\x1b_G"
            && let Some(relative_end) = sequence[i + 3..].iter().position(|&byte| byte == 0x1b)
            && sequence.get(i + 3 + relative_end + 1) == Some(&b'\\')
        {
            let body_end = i + 3 + relative_end;
            wrap_sequence_for_tmux(&sequence[i..body_end + 2], &mut out);
            i = body_end + 2;
            continue;
        }
        out.push(sequence[i]);
        i += 1;
    }
    out
}

fn wrap_sequence_for_tmux(sequence: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(b"\x1bPtmux;");
    for &byte in sequence {
        if byte == 0x1b {
            out.extend_from_slice(b"\x1b\x1b");
        } else {
            out.push(byte);
        }
    }
    out.extend_from_slice(b"\x1b\\");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_frame_sequence_transmits_raw_rgb_at_requested_area() {
        let frame = [0, 0, 0, 255, 255, 255];
        let area = ImageArea {
            x: 1,
            y: 2,
            cols: 3,
            rows: 4,
        };
        let mut out = Vec::new();
        let mut scratch = Vec::new();

        write_kitty_rgb_frame(
            &mut out,
            KittyFramePlacement {
                image_id: 7,
                placement_id: 9,
                z_index: 11,
                previous_image_id: None,
                width: 2,
                height: 1,
                area,
            },
            &frame,
            &mut scratch,
        )
        .expect("kitty frame should encode");

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\x1b[3;2H"));
        assert!(text.contains("a=T,q=2,f=24,s=2,v=1,i=7,p=9,c=3,r=4,C=1,z=11,m=0;"));
    }

    #[test]
    fn kitty_frame_sequence_chunks_large_frames() {
        let frame = vec![0x7f; KITTY_RAW_CHUNK_BYTES + 1];
        let area = ImageArea {
            x: 0,
            y: 0,
            cols: 10,
            rows: 5,
        };
        let mut out = Vec::new();
        let mut scratch = Vec::new();

        write_kitty_rgb_frame(
            &mut out,
            KittyFramePlacement {
                image_id: 7,
                placement_id: 9,
                z_index: 12,
                previous_image_id: None,
                width: 1025,
                height: 1,
                area,
            },
            &frame,
            &mut scratch,
        )
        .expect("kitty frame should encode");

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("m=1;"));
        assert!(text.contains("\x1b_Gm=0;") || text.contains("\x1b\x1b_Gm=0;"));
    }

    #[test]
    fn kitty_frame_sequence_deletes_previous_buffer_after_new_frame() {
        let frame = [0, 0, 0, 255, 255, 255];
        let area = ImageArea {
            x: 0,
            y: 0,
            cols: 2,
            rows: 1,
        };
        let mut out = Vec::new();
        let mut scratch = Vec::new();

        write_kitty_rgb_frame(
            &mut out,
            KittyFramePlacement {
                image_id: 8,
                placement_id: 9,
                z_index: 13,
                previous_image_id: Some(7),
                width: 2,
                height: 1,
                area,
            },
            &frame,
            &mut scratch,
        )
        .expect("kitty frame should encode");

        let text = String::from_utf8_lossy(&out);
        let draw = text
            .find("a=T,q=2")
            .expect("new frame draw should be present");
        let delete = text
            .find("a=d,d=I,q=2,i=7")
            .expect("old buffer delete should be present");
        assert!(draw < delete);
    }

    #[test]
    fn shutter_button_uses_transparent_rgba_kitty_image() {
        let area = ImageArea {
            x: 2,
            y: 3,
            cols: 6,
            rows: 4,
        };
        let mut out = Vec::new();
        let mut scratch = Vec::new();

        write_kitty_shutter_button(&mut out, area, CaptureMode::Photo, &mut scratch)
            .expect("shutter button should encode");

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\x1b[4;3H"));
        assert!(text.contains("a=T,q=2,f=32,s=128,v=128"));
        assert!(text.contains(&format!("i={KITTY_SHUTTER_IMAGE_ID}")));
        assert!(text.contains(&format!("p={KITTY_SHUTTER_PLACEMENT_ID}")));
    }

    #[test]
    fn shutter_button_rgba_has_transparent_corners_and_opaque_center() {
        let frame = shutter_button_rgba(16, CaptureMode::Photo);
        let corner_alpha = frame[3];
        let center = ((8 * 16 + 8) * 4) as usize;

        assert_eq!(frame.len(), 16 * 16 * 4);
        assert_eq!(corner_alpha, 0);
        assert_eq!(&frame[center..center + 3], &[250, 250, 251]);
        assert_eq!(frame[center + 3], 255);
    }

    #[test]
    fn video_mode_shutter_uses_red_inner_circle() {
        let frame = shutter_button_rgba(16, CaptureMode::Video);
        let center = ((8 * 16 + 8) * 4) as usize;

        assert_eq!(&frame[center..center + 3], &[239, 68, 68]);
        assert_eq!(frame[center + 3], 255);
    }

    #[test]
    fn mode_pill_uses_transparent_rgba_kitty_image() {
        let area = ImageArea {
            x: 1,
            y: 2,
            cols: 6,
            rows: 8,
        };
        let mut out = Vec::new();
        let mut scratch = Vec::new();

        write_kitty_mode_pill(&mut out, area, CaptureMode::Video, &mut scratch)
            .expect("mode pill should encode");

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\x1b[3;2H"));
        assert!(text.contains("a=T,q=2,f=32,s=128,v=160"));
        assert!(text.contains(&format!("i={KITTY_MODE_IMAGE_ID}")));
        assert!(text.contains(&format!("p={KITTY_MODE_PLACEMENT_ID}")));
    }

    #[test]
    fn capture_hitbox_matches_shutter_image_area() {
        let area = ImageArea {
            x: 18,
            y: 7,
            cols: 4,
            rows: 3,
        };

        assert_eq!(
            capture_hitbox(area),
            Rect {
                x: 18,
                y: 7,
                cols: 4,
                rows: 3,
            }
        );
    }

    #[test]
    fn mode_toggle_click_changes_mode_and_marks_chrome_dirty() {
        let (tx, rx) = mpsc::channel();
        let mut mode = CaptureMode::Photo;
        let mut capture_requested = false;
        let mut chrome_dirty = false;
        let layout = UiLayout {
            preview_area: ImageArea {
                x: 0,
                y: 0,
                cols: 20,
                rows: 20,
            },
            sidebar: Rect {
                x: 20,
                y: 0,
                cols: 0,
                rows: 20,
            },
            capture_button: None,
            shutter_area: None,
            mode_toggle: Some(Rect {
                x: 1,
                y: 1,
                cols: 6,
                rows: 8,
            }),
            mode_pill_area: None,
            thumbnail_area: None,
        };

        tx.send(InputEvent::Click { x: 3, y: 3 }).unwrap();

        assert!(!drain_input_events(
            &rx,
            Some(layout),
            &mut mode,
            &mut capture_requested,
            &mut chrome_dirty,
        ));
        assert_eq!(mode, CaptureMode::Video);
        assert!(!capture_requested);
        assert!(chrome_dirty);
    }
}
