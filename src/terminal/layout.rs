use crossterm::terminal;

const SIDEBAR_COLS: u16 = 16;
const MIN_PREVIEW_COLS: u16 = 20;
const MIN_SIDEBAR_COLS: u16 = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Rect {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

impl Rect {
    pub(super) fn contains(self, x: u16, y: u16) -> bool {
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
    pub(crate) recording_timer_area: Option<ImageArea>,
    pub(crate) no_mic_area: Option<ImageArea>,
    pub(crate) thumbnail_area: Option<ImageArea>,
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
    let recording_timer_area = recording_timer_area(preview_bounds);
    let no_mic_area = no_mic_area(preview_bounds);
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
        recording_timer_area,
        no_mic_area,
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

    let cols = 8.min(sidebar.cols);
    let rect = Rect {
        x: sidebar.x + sidebar.cols.saturating_sub(cols) / 2,
        y: sidebar.y + 1,
        cols,
        rows: 5,
    };
    let area = ImageArea {
        x: rect.x,
        y: rect.y,
        cols: rect.cols,
        rows: rect.rows,
    };

    (Some(rect), Some(area))
}

fn recording_timer_area(preview: Rect) -> Option<ImageArea> {
    if preview.cols < 12 || preview.rows < 4 {
        return None;
    }

    let cols = 16.min(preview.cols.saturating_sub(2));
    let rect = Rect {
        x: preview.x + preview.cols.saturating_sub(cols) / 2,
        y: preview.y + 1,
        cols,
        rows: 3,
    };

    Some(ImageArea {
        x: rect.x,
        y: rect.y,
        cols: rect.cols,
        rows: rect.rows,
    })
}

fn no_mic_area(preview: Rect) -> Option<ImageArea> {
    if preview.cols < 10 || preview.rows < 4 {
        return None;
    }

    let cols = 5.min(preview.cols.saturating_sub(2));
    Some(ImageArea {
        x: preview.x + preview.cols.saturating_sub(cols + 1),
        y: preview.y + 1,
        cols,
        rows: 3,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
