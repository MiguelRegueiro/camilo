#[derive(Clone, Copy)]
pub(super) struct Circle {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) radius: f64,
}

#[derive(Clone, Copy)]
pub(super) struct Ellipse {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) radius_x: f64,
    pub(super) radius_y: f64,
}

pub(super) fn fill_ellipse(
    frame: &mut [u8],
    width: u32,
    height: u32,
    ellipse: Ellipse,
    color: [u8; 3],
    alpha: u8,
) {
    for y in 0..height {
        for x in 0..width {
            let dx = (f64::from(x) + 0.5 - ellipse.x) / ellipse.radius_x;
            let dy = (f64::from(y) + 0.5 - ellipse.y) / ellipse.radius_y;
            let distance = (dx * dx + dy * dy).sqrt();
            let coverage =
                ((1.0 - distance) * ellipse.radius_x.min(ellipse.radius_y)).clamp(0.0, 1.0);
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

pub(super) fn fill_circle(
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

pub(super) fn fill_quad(
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

pub(super) fn fill_triangle(
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

pub(super) fn point_in_triangle(x: f64, y: f64, points: [(f64, f64); 3]) -> bool {
    let [(x1, y1), (x2, y2), (x3, y3)] = points;
    let d1 = (x - x2) * (y1 - y2) - (x1 - x2) * (y - y2);
    let d2 = (x - x3) * (y2 - y3) - (x2 - x3) * (y - y3);
    let d3 = (x - x1) * (y3 - y1) - (x3 - x1) * (y - y1);
    let has_negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_negative && has_positive)
}

#[derive(Clone, Copy)]
pub(super) struct RoundedRect {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) width: f64,
    pub(super) height: f64,
    pub(super) radius: f64,
}

pub(super) fn fill_rounded_rect(
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

pub(super) fn rounded_rect_coverage(x: f64, y: f64, rect: RoundedRect) -> f64 {
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

pub(super) fn put_pixel(frame: &mut [u8], offset: usize, color: [u8; 3], alpha: u8) {
    frame[offset] = color[0];
    frame[offset + 1] = color[1];
    frame[offset + 2] = color[2];
    frame[offset + 3] = alpha;
}

pub(super) fn blend_pixel(frame: &mut [u8], offset: usize, color: [u8; 3], alpha: u8) {
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

pub(super) fn rgba_offset(width: u32, x: u32, y: u32) -> usize {
    ((y * width + x) * 4) as usize
}

pub(super) fn edge_alpha(distance: f64, radius: f64) -> u8 {
    ((radius - distance).clamp(0.0, 1.0) * 255.0).round() as u8
}
