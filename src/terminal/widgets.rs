use std::{io, io::Write};

use super::{
    input::CaptureMode,
    kitty_graphics::{
        KITTY_MODE_IMAGE_ID, KITTY_MODE_PLACEMENT_ID, KITTY_NO_MIC_IMAGE_ID,
        KITTY_NO_MIC_PLACEMENT_ID, KITTY_SHUTTER_IMAGE_ID, KITTY_SHUTTER_PLACEMENT_ID,
        KITTY_TIMER_IMAGE_ID, KITTY_TIMER_PLACEMENT_ID, KittyFramePlacement, write_kitty_image,
    },
    layout::{ImageArea, UiLayout},
    raster::*,
};

const SHUTTER_SIZE: u32 = 128;
const MODE_PILL_WIDTH: u32 = 128;
const MODE_PILL_HEIGHT: u32 = 160;
const RECORDING_TIMER_WIDTH: u32 = 224;
const RECORDING_TIMER_HEIGHT: u32 = 48;
const NO_MIC_PILL_WIDTH: u32 = 72;
const NO_MIC_PILL_HEIGHT: u32 = 48;
const SHUTTER_Z_INDEX: i32 = 1_000_000_001;
const MODE_PILL_Z_INDEX: i32 = 1_000_000_002;
const TIMER_Z_INDEX: i32 = 1_000_000_003;
const NO_MIC_Z_INDEX: i32 = 1_000_000_004;

pub(crate) fn draw_sidebar(out: &mut impl Write, layout: UiLayout) -> io::Result<()> {
    if layout.sidebar.cols == 0 {
        return Ok(());
    }

    out.write_all(b"\x1b[0m")
}

pub(crate) fn write_kitty_shutter_button(
    out: &mut impl Write,
    area: ImageArea,
    capture_mode: CaptureMode,
    recording: bool,
    sequence: &mut Vec<u8>,
) -> io::Result<()> {
    let frame = shutter_button_rgba(SHUTTER_SIZE, capture_mode, recording);
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

pub(crate) fn write_kitty_recording_timer(
    out: &mut impl Write,
    area: ImageArea,
    elapsed: std::time::Duration,
    sequence: &mut Vec<u8>,
) -> io::Result<()> {
    let frame = recording_timer_rgba(RECORDING_TIMER_WIDTH, RECORDING_TIMER_HEIGHT, elapsed);
    write_kitty_image(
        out,
        KittyFramePlacement {
            image_id: KITTY_TIMER_IMAGE_ID,
            placement_id: KITTY_TIMER_PLACEMENT_ID,
            z_index: TIMER_Z_INDEX,
            previous_image_id: None,
            width: RECORDING_TIMER_WIDTH,
            height: RECORDING_TIMER_HEIGHT,
            area,
        },
        &frame,
        32,
        sequence,
    )
}

pub(crate) fn write_kitty_no_mic_pill(
    out: &mut impl Write,
    area: ImageArea,
    sequence: &mut Vec<u8>,
) -> io::Result<()> {
    let frame = no_mic_pill_rgba(NO_MIC_PILL_WIDTH, NO_MIC_PILL_HEIGHT);
    write_kitty_image(
        out,
        KittyFramePlacement {
            image_id: KITTY_NO_MIC_IMAGE_ID,
            placement_id: KITTY_NO_MIC_PLACEMENT_ID,
            z_index: NO_MIC_Z_INDEX,
            previous_image_id: None,
            width: NO_MIC_PILL_WIDTH,
            height: NO_MIC_PILL_HEIGHT,
            area,
        },
        &frame,
        32,
        sequence,
    )
}

fn recording_timer_rgba(width: u32, height: u32, elapsed: std::time::Duration) -> Vec<u8> {
    let mut frame = vec![0_u8; (width * height * 4) as usize];
    fill_rounded_rect(
        &mut frame,
        width,
        height,
        RoundedRect {
            x: 4.0,
            y: 4.0,
            width: f64::from(width - 8),
            height: f64::from(height - 8),
            radius: 24.0,
        },
        [24, 24, 27],
        168,
    );
    fill_ellipse(
        &mut frame,
        width,
        height,
        Ellipse {
            x: 44.0,
            y: f64::from(height) / 2.0,
            radius_x: 10.5,
            radius_y: 5.0,
        },
        [239, 68, 68],
        245,
    );

    let total_seconds = elapsed.as_secs().min(99 * 60 + 59);
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    let text = format!("{minutes:02}:{seconds:02}");
    draw_timer_text(&mut frame, width, height, 72, 11, &text);
    frame
}

fn no_mic_pill_rgba(width: u32, height: u32) -> Vec<u8> {
    let mut frame = vec![0_u8; (width * height * 4) as usize];
    fill_rounded_rect(
        &mut frame,
        width,
        height,
        RoundedRect {
            x: 8.0,
            y: 5.0,
            width: f64::from(width - 16),
            height: f64::from(height - 10),
            radius: 18.0,
        },
        [24, 24, 27],
        176,
    );
    draw_mic_glyph(&mut frame, width, height, [250, 250, 250]);
    draw_soft_line(
        &mut frame,
        width,
        height,
        SoftLine {
            from: (27.0, 33.0),
            to: (46.0, 15.0),
            radius: 3.0,
            color: [239, 68, 68],
            alpha: 245,
        },
    );
    frame
}

fn draw_mic_glyph(frame: &mut [u8], width: u32, height: u32, color: [u8; 3]) {
    let center_x = f64::from(width) / 2.0;
    fill_rounded_rect(
        frame,
        width,
        height,
        RoundedRect {
            x: center_x - 6.5,
            y: 12.0,
            width: 13.0,
            height: 15.0,
            radius: 6.5,
        },
        color,
        238,
    );
    draw_mic_arc(frame, width, height, center_x, color);
    fill_rounded_rect(
        frame,
        width,
        height,
        RoundedRect {
            x: center_x - 1.75,
            y: 30.0,
            width: 3.5,
            height: 4.0,
            radius: 2.0,
        },
        color,
        238,
    );
}

fn draw_mic_arc(frame: &mut [u8], width: u32, height: u32, center_x: f64, color: [u8; 3]) {
    let center_y = 23.0;
    let radius_x = 11.0;
    let radius_y = 6.5;
    for y in 0..height {
        for x in 0..width {
            let px = f64::from(x) + 0.5;
            let py = f64::from(y) + 0.5;
            let dx = (px - center_x) / radius_x;
            let dy = (py - center_y) / radius_y;
            if py < center_y || py > center_y + radius_y + 1.0 {
                continue;
            }
            let distance = (dx * dx + dy * dy).sqrt();
            let coverage = (1.65 - (distance - 1.0).abs() * radius_x.min(radius_y)).clamp(0.0, 1.0);
            if coverage > 0.0 {
                let offset = rgba_offset(width, x, y);
                blend_pixel(frame, offset, color, (coverage * 220.0).round() as u8);
            }
        }
    }
}

struct SoftLine {
    from: (f64, f64),
    to: (f64, f64),
    radius: f64,
    color: [u8; 3],
    alpha: u8,
}

fn draw_soft_line(frame: &mut [u8], width: u32, height: u32, line: SoftLine) {
    let (x0, y0) = line.from;
    let (x1, y1) = line.to;
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len_sq = dx * dx + dy * dy;
    if len_sq == 0.0 {
        return;
    }
    for y in 0..height {
        for x in 0..width {
            let px = f64::from(x) + 0.5;
            let py = f64::from(y) + 0.5;
            let t = (((px - x0) * dx + (py - y0) * dy) / len_sq).clamp(0.0, 1.0);
            let nearest_x = x0 + t * dx;
            let nearest_y = y0 + t * dy;
            let distance = ((px - nearest_x).powi(2) + (py - nearest_y).powi(2)).sqrt();
            let coverage = (line.radius - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                let offset = rgba_offset(width, x, y);
                blend_pixel(
                    frame,
                    offset,
                    line.color,
                    (coverage * f64::from(line.alpha)).round() as u8,
                );
            }
        }
    }
}

fn draw_timer_text(frame: &mut [u8], width: u32, height: u32, x: u32, y: u32, text: &str) {
    let mut cursor = x;
    for ch in text.bytes() {
        match ch {
            b'0'..=b'9' => {
                draw_timer_digit(frame, width, height, cursor, y, ch - b'0');
                cursor += 22;
            }
            b':' => {
                fill_timer_bar(frame, width, height, cursor + 3, y + 8, 4, 4);
                fill_timer_bar(frame, width, height, cursor + 3, y + 20, 4, 4);
                cursor += 12;
            }
            _ => {}
        }
    }
}

fn draw_timer_digit(frame: &mut [u8], width: u32, height: u32, x: u32, y: u32, digit: u8) {
    const DIGITS: [u32; 10] = [
        0b111_101_101_101_101_101_111,
        0b010_110_010_010_010_010_111,
        0b111_001_001_111_100_100_111,
        0b111_001_001_111_001_001_111,
        0b101_101_101_111_001_001_001,
        0b111_100_100_111_001_001_111,
        0b111_100_100_111_101_101_111,
        0b111_001_001_010_010_010_010,
        0b111_101_101_111_101_101_111,
        0b111_101_101_111_001_001_111,
    ];
    let Some(mask) = DIGITS.get(digit as usize).copied() else {
        return;
    };
    for row in 0..7 {
        for col in 0..3 {
            let bit = 20 - (row * 3 + col);
            if (mask & (1 << bit)) != 0 {
                fill_timer_bar(frame, width, height, x + col * 6, y + row * 4, 4, 3);
            }
        }
    }
}

fn fill_timer_bar(frame: &mut [u8], width: u32, height: u32, x: u32, y: u32, cols: u32, rows: u32) {
    for py in y..y.saturating_add(rows).min(height) {
        for px in x..x.saturating_add(cols).min(width) {
            let offset = rgba_offset(width, px, py);
            blend_pixel(frame, offset, [250, 250, 250], 218);
        }
    }
}

fn shutter_button_rgba(size: u32, capture_mode: CaptureMode, recording: bool) -> Vec<u8> {
    let mut frame = vec![0_u8; (size * size * 4) as usize];
    let center = (f64::from(size) - 1.0) / 2.0;
    let outer_radius = f64::from(size) * 0.46;
    let inner_radius = f64::from(size) * 0.34;
    let gap_radius = f64::from(size) * 0.39;
    let inner_color = match capture_mode {
        CaptureMode::Photo => [250, 250, 251],
        CaptureMode::Video => [239, 68, 68],
    };
    let stop_square = RoundedRect {
        x: center - f64::from(size) * 0.20,
        y: center - f64::from(size) * 0.20,
        width: f64::from(size) * 0.40,
        height: f64::from(size) * 0.40,
        radius: f64::from(size) * 0.045,
    };

    for y in 0..size {
        for x in 0..size {
            let dx = f64::from(x) - center;
            let dy = f64::from(y) - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let offset = rgba_offset(size, x, y);

            let stop_coverage = if recording {
                rounded_rect_coverage(f64::from(x) + 0.5, f64::from(y) + 0.5, stop_square)
            } else {
                0.0
            };
            let color = if stop_coverage > 0.0 {
                Some((inner_color, (stop_coverage * 255.0).round() as u8))
            } else if !recording && distance <= inner_radius {
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

#[cfg(test)]
mod tests {
    use super::*;

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

        write_kitty_shutter_button(&mut out, area, CaptureMode::Photo, false, &mut scratch)
            .expect("shutter button should encode");

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\x1b[4;3H"));
        assert!(text.contains("a=T,q=2,f=32,s=128,v=128"));
        assert!(text.contains(&format!("i={KITTY_SHUTTER_IMAGE_ID}")));
        assert!(text.contains(&format!("p={KITTY_SHUTTER_PLACEMENT_ID}")));
    }

    #[test]
    fn shutter_button_rgba_has_transparent_corners_and_opaque_center() {
        let frame = shutter_button_rgba(16, CaptureMode::Photo, false);
        let corner_alpha = frame[3];
        let center = ((8 * 16 + 8) * 4) as usize;

        assert_eq!(frame.len(), 16 * 16 * 4);
        assert_eq!(corner_alpha, 0);
        assert_eq!(&frame[center..center + 3], &[250, 250, 251]);
        assert_eq!(frame[center + 3], 255);
    }

    #[test]
    fn video_mode_shutter_uses_red_inner_circle() {
        let frame = shutter_button_rgba(16, CaptureMode::Video, false);
        let center = ((8 * 16 + 8) * 4) as usize;

        assert_eq!(&frame[center..center + 3], &[239, 68, 68]);
        assert_eq!(frame[center + 3], 255);
    }

    #[test]
    fn recording_shutter_uses_red_stop_square() {
        let frame = shutter_button_rgba(32, CaptureMode::Video, true);
        let center = ((16 * 32 + 16) * 4) as usize;
        let outer_inner_circle_point = ((16 * 32 + 6) * 4) as usize;
        let outer_ring_point = ((16 * 32 + 3) * 4) as usize;

        assert_eq!(&frame[center..center + 3], &[239, 68, 68]);
        assert_eq!(frame[center + 3], 255);
        assert_eq!(frame[outer_inner_circle_point + 3], 0);
        assert!(frame[outer_ring_point + 3] > 0);
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
    fn recording_timer_uses_transparent_rgba_kitty_image() {
        let area = ImageArea {
            x: 3,
            y: 1,
            cols: 16,
            rows: 3,
        };
        let mut out = Vec::new();
        let mut scratch = Vec::new();

        write_kitty_recording_timer(
            &mut out,
            area,
            std::time::Duration::from_secs(65),
            &mut scratch,
        )
        .expect("recording timer should encode");

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\x1b[2;4H"));
        assert!(text.contains("a=T,q=2,f=32,s=224,v=48"));
        assert!(text.contains(&format!("i={KITTY_TIMER_IMAGE_ID}")));
        assert!(text.contains(&format!("p={KITTY_TIMER_PLACEMENT_ID}")));
    }

    #[test]
    fn no_mic_pill_uses_transparent_rgba_kitty_image() {
        let area = ImageArea {
            x: 3,
            y: 1,
            cols: 5,
            rows: 3,
        };
        let mut out = Vec::new();
        let mut scratch = Vec::new();

        write_kitty_no_mic_pill(&mut out, area, &mut scratch).expect("no mic pill should encode");

        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\x1b[2;4H"));
        assert!(text.contains("a=T,q=2,f=32,s=72,v=48"));
        assert!(text.contains(&format!("i={KITTY_NO_MIC_IMAGE_ID}")));
        assert!(text.contains(&format!("p={KITTY_NO_MIC_PLACEMENT_ID}")));
    }
}
