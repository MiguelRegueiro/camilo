use crossterm::terminal;

const SIDEBAR_COLS: u16 = 16;
const MIN_PREVIEW_COLS: u16 = 20;
const MIN_SIDEBAR_COLS: u16 = 12;
const SIDEBAR_CONTROL_COLS: u16 = 8;
const MODE_PILL_Y: u16 = 1;
const MODE_PILL_ROWS: u16 = 5;
const SELF_TIMER_Y: u16 = 6;
const SELF_TIMER_ROWS: u16 = 3;
const SHUTTER_SLOT_ROWS: u16 = 5;
const CONTROL_GAP_ROWS: u16 = 1;
const MIN_THUMBNAIL_ROWS: u16 = 2;

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
    pub(crate) self_timer_toggle: Option<Rect>,
    pub(crate) self_timer_area: Option<ImageArea>,
    pub(crate) countdown_area: Option<ImageArea>,
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

    let (mode_toggle, mode_pill_area) = mode_pill_area(sidebar);
    let (self_timer_toggle, self_timer_area) = self_timer_area(sidebar);
    let countdown_area = countdown_area(preview_bounds);
    let recording_timer_area = recording_timer_area(preview_bounds);
    let no_mic_area = no_mic_area(preview_bounds);
    let shutter_slot = shutter_slot(sidebar, rows, self_timer_area);
    let shutter_area = shutter_slot.and_then(|slot| shutter_area(slot, cell_width, cell_height));
    let capture_button = shutter_area.map(capture_hitbox);
    let thumbnail_area = thumbnail_area(sidebar, rows, cell_width, cell_height, shutter_area);

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
        self_timer_toggle,
        self_timer_area,
        countdown_area,
        recording_timer_area,
        no_mic_area,
        thumbnail_area,
    }
}

pub(crate) fn image_area_pixel_size(area: ImageArea) -> (u32, u32) {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let (pixel_width, pixel_height) = terminal_pixel_size(cols, rows);
    let cell_width = f64::from(pixel_width) / f64::from(cols.max(1));
    let cell_height = f64::from(pixel_height) / f64::from(rows.max(1));
    (
        ((f64::from(area.cols.max(1)) * cell_width).round() as u32).max(1),
        ((f64::from(area.rows.max(1)) * cell_height).round() as u32).max(1),
    )
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

    let cols = SIDEBAR_CONTROL_COLS.min(sidebar.cols);
    let rect = Rect {
        x: sidebar.x + sidebar.cols.saturating_sub(cols) / 2,
        y: sidebar.y + MODE_PILL_Y,
        cols,
        rows: MODE_PILL_ROWS,
    };
    let area = ImageArea {
        x: rect.x,
        y: rect.y,
        cols: rect.cols,
        rows: rect.rows,
    };

    (Some(rect), Some(area))
}

fn self_timer_area(sidebar: Rect) -> (Option<Rect>, Option<ImageArea>) {
    if sidebar.cols < 6 || sidebar.rows < 12 {
        return (None, None);
    }

    let cols = SIDEBAR_CONTROL_COLS.min(sidebar.cols);
    let rect = Rect {
        x: sidebar.x + sidebar.cols.saturating_sub(cols) / 2,
        y: sidebar.y + SELF_TIMER_Y,
        cols,
        rows: SELF_TIMER_ROWS,
    };
    let area = ImageArea {
        x: rect.x,
        y: rect.y,
        cols: rect.cols,
        rows: rect.rows,
    };

    (Some(rect), Some(area))
}

fn shutter_slot(
    sidebar: Rect,
    terminal_rows: u16,
    self_timer_area: Option<ImageArea>,
) -> Option<Rect> {
    if sidebar.cols == 0 || terminal_rows == 0 {
        return None;
    }

    let rows = SHUTTER_SLOT_ROWS.min(terminal_rows.max(1));
    let ideal_y = terminal_rows.saturating_sub(rows) / 2;
    let min_y = self_timer_area
        .map(|area| {
            area.y
                .saturating_add(area.rows)
                .saturating_add(CONTROL_GAP_ROWS)
        })
        .unwrap_or(0);
    let max_y = terminal_rows.saturating_sub(rows);

    Some(Rect {
        x: sidebar.x,
        y: ideal_y.max(min_y).min(max_y),
        cols: sidebar.cols,
        rows,
    })
}

fn countdown_area(preview: Rect) -> Option<ImageArea> {
    if preview.cols < 8 || preview.rows < 6 {
        return None;
    }

    let cols = 8.min(preview.cols.saturating_sub(2));
    let rows = 5.min(preview.rows.saturating_sub(2));
    Some(ImageArea {
        x: preview.x + preview.cols.saturating_sub(cols) / 2,
        y: preview.y + preview.rows.saturating_sub(rows) / 2,
        cols,
        rows,
    })
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
    shutter_area: Option<ImageArea>,
) -> Option<ImageArea> {
    if sidebar.cols < 4 || terminal_rows < 4 {
        return None;
    }

    let max_y = shutter_area
        .map(|shutter| {
            shutter
                .y
                .saturating_add(shutter.rows)
                .saturating_add(CONTROL_GAP_ROWS)
        })
        .unwrap_or(1);
    let available_rows = terminal_rows.saturating_sub(max_y.saturating_add(1));
    if available_rows < MIN_THUMBNAIL_ROWS {
        return None;
    }

    let max_cols_by_sidebar = sidebar.cols.saturating_sub(2).max(1);
    let max_cols_by_height =
        ((f64::from(available_rows) * cell_height / cell_width).floor() as u16).max(1);
    let thumb_cols = max_cols_by_sidebar.min(max_cols_by_height);
    let thumb_rows = ((f64::from(thumb_cols) * cell_width / cell_height).round() as u16)
        .clamp(1, available_rows);

    let area = ImageArea {
        x: sidebar.x + sidebar.cols.saturating_sub(thumb_cols) / 2,
        y: terminal_rows.saturating_sub(thumb_rows + 1),
        cols: thumb_cols,
        rows: thumb_rows,
    };

    Some(area)
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

    #[test]
    fn compact_sidebar_moves_shutter_below_full_height_timer() {
        let sidebar = Rect {
            x: 20,
            y: 0,
            cols: 16,
            rows: 20,
        };
        let (_, timer) = self_timer_area(sidebar);
        let timer = timer.unwrap();

        let shutter = shutter_slot(sidebar, 20, Some(timer)).unwrap();

        assert_eq!(shutter.rows, 5);
        assert_eq!(shutter.y, 10);
        assert!(timer.y + timer.rows < shutter.y);
    }

    #[test]
    fn shutter_wins_over_thumbnail_when_sidebar_is_cramped() {
        let sidebar = Rect {
            x: 20,
            y: 0,
            cols: 16,
            rows: 16,
        };
        let (_, timer) = self_timer_area(sidebar);
        let timer = timer.unwrap();

        let shutter = shutter_slot(sidebar, 16, Some(timer)).unwrap();
        let thumbnail = thumbnail_area(
            sidebar,
            16,
            8.0,
            16.0,
            Some(ImageArea {
                x: 24,
                y: shutter.y,
                cols: 8,
                rows: shutter.rows,
            }),
        );

        assert_eq!(shutter.y, 10);
        assert!(thumbnail.is_none());
    }

    #[test]
    fn thumbnail_shrinks_to_remaining_space_below_shutter() {
        let sidebar = Rect {
            x: 20,
            y: 0,
            cols: 16,
            rows: 20,
        };
        let shutter = ImageArea {
            x: 24,
            y: 10,
            cols: 8,
            rows: 5,
        };

        let thumbnail = thumbnail_area(sidebar, 20, 8.0, 16.0, Some(shutter)).unwrap();

        assert!(thumbnail.rows <= 3);
        assert!(thumbnail.y > shutter.y + shutter.rows);
        assert!(thumbnail.cols < sidebar.cols.saturating_sub(2));
    }
}
