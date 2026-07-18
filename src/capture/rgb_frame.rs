use anyhow::{Result, anyhow};
use zune_jpeg::{JpegDecoder, zune_core::bytestream::ZCursor};

pub(super) const RAW_RGB_BYTES_PER_PIXEL: usize = 3;

pub(crate) fn frame_len(width: u32, height: u32) -> Result<usize> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| anyhow!("frame dimensions are too large"))?;
    pixels
        .checked_mul(RAW_RGB_BYTES_PER_PIXEL as u32)
        .map(|v| v as usize)
        .ok_or_else(|| anyhow!("frame buffer is too large"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum V4l2PixelFormat {
    Rgb24,
    Yuyv,
    Mjpeg,
}

impl V4l2PixelFormat {
    pub(super) fn fourcc(self) -> u32 {
        v4l2_fourcc(self.fourcc_bytes())
    }

    pub(super) fn fourcc_bytes(self) -> [u8; 4] {
        match self {
            Self::Rgb24 => *b"RGB3",
            Self::Yuyv => *b"YUYV",
            Self::Mjpeg => *b"MJPG",
        }
    }

    pub(super) fn from_fourcc(fourcc: u32) -> Option<Self> {
        if fourcc == Self::Rgb24.fourcc() {
            Some(Self::Rgb24)
        } else if fourcc == Self::Yuyv.fourcc() {
            Some(Self::Yuyv)
        } else if fourcc == Self::Mjpeg.fourcc() {
            Some(Self::Mjpeg)
        } else {
            None
        }
    }

    pub(super) fn fourcc_name(self) -> &'static str {
        match self {
            Self::Rgb24 => "RGB3",
            Self::Yuyv => "YUYV",
            Self::Mjpeg => "MJPG",
        }
    }

    pub(super) fn input_format(self) -> &'static str {
        match self {
            Self::Rgb24 => "rgb24",
            Self::Yuyv => "yuyv422",
            Self::Mjpeg => "mjpeg",
        }
    }
}

pub(super) const fn v4l2_fourcc(bytes: [u8; 4]) -> u32 {
    bytes[0] as u32
        | ((bytes[1] as u32) << 8)
        | ((bytes[2] as u32) << 16)
        | ((bytes[3] as u32) << 24)
}

pub(super) fn decode_camera_frame(
    pixel_format: V4l2PixelFormat,
    frame: &[u8],
    width: u32,
    height: u32,
    out: &mut [u8],
) -> std::result::Result<(), String> {
    match pixel_format {
        V4l2PixelFormat::Rgb24 => copy_rgb24_frame(frame, out),
        V4l2PixelFormat::Yuyv => convert_yuyv_to_rgb24(frame, width, height, out),
        V4l2PixelFormat::Mjpeg => decode_mjpeg_to_rgb24(frame, width, height, out),
    }
}

fn copy_rgb24_frame(frame: &[u8], out: &mut [u8]) -> std::result::Result<(), String> {
    if frame.len() < out.len() {
        return Err(format!(
            "rgb24 frame has {} bytes, expected at least {}",
            frame.len(),
            out.len()
        ));
    }
    out.copy_from_slice(&frame[..out.len()]);
    Ok(())
}

pub(super) fn convert_yuyv_to_rgb24(
    frame: &[u8],
    width: u32,
    height: u32,
    out: &mut [u8],
) -> std::result::Result<(), String> {
    let expected_in = width as usize * height as usize * 2;
    let expected_out = width as usize * height as usize * RAW_RGB_BYTES_PER_PIXEL;
    if frame.len() < expected_in || out.len() < expected_out {
        return Err(format!(
            "yuyv frame has {} bytes and output has {}, expected {expected_in}/{expected_out}",
            frame.len(),
            out.len()
        ));
    }

    let mut dst = 0;
    for chunk in frame[..expected_in].chunks_exact(4) {
        let y0 = chunk[0];
        let u = chunk[1];
        let y1 = chunk[2];
        let v = chunk[3];
        let [r, g, b] = yuv_to_rgb(y0, u, v);
        out[dst..dst + 3].copy_from_slice(&[r, g, b]);
        let [r, g, b] = yuv_to_rgb(y1, u, v);
        out[dst + 3..dst + 6].copy_from_slice(&[r, g, b]);
        dst += 6;
    }
    Ok(())
}

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = i32::from(y).saturating_sub(16).max(0);
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    [
        clamp_rgb((298 * c + 409 * e + 128) >> 8),
        clamp_rgb((298 * c - 100 * d - 208 * e + 128) >> 8),
        clamp_rgb((298 * c + 516 * d + 128) >> 8),
    ]
}

fn clamp_rgb(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn decode_mjpeg_to_rgb24(
    frame: &[u8],
    width: u32,
    height: u32,
    out: &mut [u8],
) -> std::result::Result<(), String> {
    let mut decoder = JpegDecoder::new(ZCursor::new(frame));
    decoder
        .decode_headers()
        .map_err(|error| format!("failed to decode mjpeg headers: {error}"))?;
    let info = decoder
        .info()
        .ok_or_else(|| "mjpeg frame did not report dimensions".to_string())?;
    if usize::from(info.width) != width as usize || usize::from(info.height) != height as usize {
        return Err(format!(
            "mjpeg frame is {}x{}, expected {}x{}",
            info.width, info.height, width, height
        ));
    }
    let decoded_len = decoder
        .output_buffer_size()
        .ok_or_else(|| "mjpeg frame did not report an output size".to_string())?;
    if decoded_len != out.len() {
        return Err(format!(
            "mjpeg decoded to {} bytes, expected {} rgb bytes",
            decoded_len,
            out.len()
        ));
    }
    decoder
        .decode_into(out)
        .map_err(|error| format!("failed to decode mjpeg frame: {error}"))
}

pub(super) fn mirror_rgb24_in_place(frame: &mut [u8], width: u32, height: u32) {
    let width = width as usize;
    let height = height as usize;
    let stride = width * RAW_RGB_BYTES_PER_PIXEL;
    for y in 0..height {
        let row = y * stride;
        for x in 0..width / 2 {
            let left = row + x * RAW_RGB_BYTES_PER_PIXEL;
            let right = row + (width - 1 - x) * RAW_RGB_BYTES_PER_PIXEL;
            for channel in 0..RAW_RGB_BYTES_PER_PIXEL {
                frame.swap(left + channel, right + channel);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScaleDimensions {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
}

#[derive(Clone, Copy, Debug)]
struct AxisSample {
    lower_offset: usize,
    upper_offset: usize,
    weight: f64,
}

#[derive(Default)]
pub(crate) struct Rgb24Scaler {
    dimensions: Option<ScaleDimensions>,
    horizontal: Vec<AxisSample>,
    vertical: Vec<AxisSample>,
}

impl Rgb24Scaler {
    pub(crate) fn resize(
        &mut self,
        src: &[u8],
        src_width: u32,
        src_height: u32,
        dst: &mut [u8],
        dst_width: u32,
        dst_height: u32,
    ) -> Result<()> {
        if src_width == 0 || src_height == 0 || dst_width == 0 || dst_height == 0 {
            return Err(anyhow!("resize dimensions must be greater than zero"));
        }
        let expected_src = frame_len(src_width, src_height)?;
        let expected_dst = frame_len(dst_width, dst_height)?;
        if src.len() != expected_src || dst.len() != expected_dst {
            return Err(anyhow!(
                "resize buffers have {}/{} bytes, expected {expected_src}/{expected_dst}",
                src.len(),
                dst.len()
            ));
        }
        if src_width == dst_width && src_height == dst_height {
            dst.copy_from_slice(src);
            return Ok(());
        }

        let dimensions = ScaleDimensions {
            src_width,
            src_height,
            dst_width,
            dst_height,
        };
        self.prepare(dimensions);

        let src_stride = src_width as usize * RAW_RGB_BYTES_PER_PIXEL;
        let dst_stride = dst_width as usize * RAW_RGB_BYTES_PER_PIXEL;
        for (dst_row, vertical) in dst.chunks_exact_mut(dst_stride).zip(&self.vertical) {
            let top_row = &src[vertical.lower_offset..vertical.lower_offset + src_stride];
            let bottom_row = &src[vertical.upper_offset..vertical.upper_offset + src_stride];
            for (dst_pixel, horizontal) in dst_row
                .chunks_exact_mut(RAW_RGB_BYTES_PER_PIXEL)
                .zip(&self.horizontal)
            {
                for channel in 0..RAW_RGB_BYTES_PER_PIXEL {
                    // SAFETY: `prepare` constrains both horizontal offsets to complete
                    // RGB pixels inside each source row. The destination comes from
                    // exact three-byte chunks, and `channel` is always in 0..3.
                    unsafe {
                        let top_left =
                            *top_row.get_unchecked(horizontal.lower_offset + channel) as f64;
                        let top_right =
                            *top_row.get_unchecked(horizontal.upper_offset + channel) as f64;
                        let bottom_left =
                            *bottom_row.get_unchecked(horizontal.lower_offset + channel) as f64;
                        let bottom_right =
                            *bottom_row.get_unchecked(horizontal.upper_offset + channel) as f64;
                        let top = top_left + (top_right - top_left) * horizontal.weight;
                        let bottom = bottom_left + (bottom_right - bottom_left) * horizontal.weight;
                        *dst_pixel.get_unchecked_mut(channel) =
                            (top + (bottom - top) * vertical.weight).round() as u8;
                    }
                }
            }
        }

        Ok(())
    }

    fn prepare(&mut self, dimensions: ScaleDimensions) {
        if self.dimensions == Some(dimensions) {
            return;
        }

        prepare_axis_samples(
            &mut self.horizontal,
            dimensions.src_width,
            dimensions.dst_width,
            RAW_RGB_BYTES_PER_PIXEL,
        );
        prepare_axis_samples(
            &mut self.vertical,
            dimensions.src_height,
            dimensions.dst_height,
            dimensions.src_width as usize * RAW_RGB_BYTES_PER_PIXEL,
        );
        self.dimensions = Some(dimensions);
    }
}

fn prepare_axis_samples(
    samples: &mut Vec<AxisSample>,
    source_len: u32,
    target_len: u32,
    offset_step: usize,
) {
    samples.clear();
    samples.reserve(target_len as usize);
    let scale = f64::from(source_len) / f64::from(target_len);
    let source_last = source_len as usize - 1;
    samples.extend((0..target_len as usize).map(|target| {
        let source = ((target as f64 + 0.5) * scale - 0.5).clamp(0.0, source_last as f64);
        let lower = source.floor() as usize;
        let upper = (lower + 1).min(source_last);
        AxisSample {
            lower_offset: lower * offset_step,
            upper_offset: upper * offset_step,
            weight: source - lower as f64,
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MJPEG_BASE64: &str = concat!(
        "/9j/4AAQSkZJRgABAgAAAQABAAD//gAQTGF2YzYyLjI4LjEwMgD/2wBDAAgEBAQEBAUF",
        "BQUFBQYGBgYGBgYGBgYGBgYHBwcICAgHBwcGBgcHCAgICAkJCQgICAgJCQoKCgwMCwsOD",
        "g4RERT/xABMAAEBAAAAAAAAAAAAAAAAAAAABwEBAQAAAAAAAAAAAAAAAAAABQYQAQAAAAAA",
        "AAAAAAAAAAAAAAARAQAAAAAAAAAAAAAAAAAAAAD/wAARCAACAAQDASIAAhEAAxEA/9oADA",
        "MBAAIRAxEAPwCegKUS/9k="
    );

    #[test]
    fn yuyv_conversion_outputs_rgb_pairs() {
        let frame = [16, 128, 235, 128];
        let mut out = [0_u8; 6];

        convert_yuyv_to_rgb24(&frame, 2, 1, &mut out).expect("yuyv should convert");

        assert_eq!(out, [0, 0, 0, 255, 255, 255]);
    }

    #[test]
    fn rgb_mirror_flips_rows_in_place() {
        let mut frame = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

        mirror_rgb24_in_place(&mut frame, 2, 2);

        assert_eq!(frame, vec![4, 5, 6, 1, 2, 3, 10, 11, 12, 7, 8, 9]);
    }

    #[test]
    fn mjpeg_decode_into_matches_allocating_decoder_output() {
        let frame = base64_simd::STANDARD
            .decode_to_vec(TEST_MJPEG_BASE64)
            .expect("test jpeg should decode from base64");
        let mut reference_decoder = JpegDecoder::new(ZCursor::new(&frame));
        let expected = reference_decoder
            .decode()
            .expect("reference jpeg decode should succeed");
        let mut actual = vec![0_u8; frame_len(4, 2).unwrap()];

        decode_mjpeg_to_rgb24(&frame, 4, 2, &mut actual)
            .expect("in-place jpeg decode should succeed");

        assert_eq!(actual, expected);
    }

    #[test]
    fn mjpeg_decode_rejects_unexpected_dimensions_before_writing_output() {
        let frame = base64_simd::STANDARD
            .decode_to_vec(TEST_MJPEG_BASE64)
            .expect("test jpeg should decode from base64");
        let mut out = vec![0x5a; frame_len(2, 4).unwrap()];

        let error = decode_mjpeg_to_rgb24(&frame, 2, 4, &mut out)
            .expect_err("mismatched jpeg dimensions should fail");

        assert!(error.contains("mjpeg frame is 4x2, expected 2x4"));
        assert!(out.iter().all(|&byte| byte == 0x5a));
    }

    #[test]
    fn reusable_scaler_is_pixel_exact_with_original_bilinear_scaling() {
        let mut scaler = Rgb24Scaler::default();
        for src_width in 1..=8 {
            for src_height in 1..=8 {
                let src = patterned_frame(src_width, src_height);
                for dst_width in 1..=8 {
                    for dst_height in 1..=8 {
                        let mut expected = vec![0_u8; frame_len(dst_width, dst_height).unwrap()];
                        let mut actual = vec![0_u8; expected.len()];
                        reference_resize_rgb24(
                            &src,
                            src_width,
                            src_height,
                            &mut expected,
                            dst_width,
                            dst_height,
                        );
                        scaler
                            .resize(
                                &src,
                                src_width,
                                src_height,
                                &mut actual,
                                dst_width,
                                dst_height,
                            )
                            .unwrap();

                        assert_eq!(
                            actual, expected,
                            "{src_width}x{src_height} -> {dst_width}x{dst_height}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn reusable_scaler_matches_original_for_larger_uneven_dimensions() {
        for (src_width, src_height, dst_width, dst_height) in
            [(13, 7, 8, 5), (8, 5, 13, 7), (192, 108, 127, 71)]
        {
            let src = patterned_frame(src_width, src_height);
            let mut expected = vec![0_u8; frame_len(dst_width, dst_height).unwrap()];
            let mut actual = vec![0_u8; expected.len()];
            reference_resize_rgb24(
                &src,
                src_width,
                src_height,
                &mut expected,
                dst_width,
                dst_height,
            );
            Rgb24Scaler::default()
                .resize(
                    &src,
                    src_width,
                    src_height,
                    &mut actual,
                    dst_width,
                    dst_height,
                )
                .unwrap();

            assert_eq!(
                actual, expected,
                "{src_width}x{src_height} -> {dst_width}x{dst_height}"
            );
        }
    }

    #[test]
    fn reusable_scaler_reuses_mapping_storage_and_reconfigures_exactly() {
        let mut scaler = Rgb24Scaler::default();
        let src = patterned_frame(8, 6);
        let mut first = vec![0_u8; frame_len(6, 4).unwrap()];
        scaler.resize(&src, 8, 6, &mut first, 6, 4).unwrap();
        let horizontal_ptr = scaler.horizontal.as_ptr();
        let vertical_ptr = scaler.vertical.as_ptr();

        let mut second = vec![0_u8; first.len()];
        scaler.resize(&src, 8, 6, &mut second, 6, 4).unwrap();

        assert_eq!(first, second);
        assert_eq!(scaler.horizontal.as_ptr(), horizontal_ptr);
        assert_eq!(scaler.vertical.as_ptr(), vertical_ptr);

        let mut resized = vec![0_u8; frame_len(3, 2).unwrap()];
        let mut expected = vec![0_u8; resized.len()];
        scaler.resize(&src, 8, 6, &mut resized, 3, 2).unwrap();
        reference_resize_rgb24(&src, 8, 6, &mut expected, 3, 2);
        assert_eq!(resized, expected);
    }

    #[test]
    fn reusable_scaler_validates_dimensions_and_buffers() {
        let mut scaler = Rgb24Scaler::default();
        let error = scaler
            .resize(&[], 0, 1, &mut [], 1, 1)
            .expect_err("zero dimensions should fail");
        assert!(error.to_string().contains("greater than zero"));

        let error = scaler
            .resize(&[0; 3], 1, 1, &mut [0; 5], 1, 1)
            .expect_err("incorrect buffer sizes should fail");
        assert!(error.to_string().contains("resize buffers"));
    }

    fn patterned_frame(width: u32, height: u32) -> Vec<u8> {
        (0..frame_len(width, height).unwrap())
            .map(|index| ((index * 37 + index / 11) & 0xff) as u8)
            .collect()
    }

    fn reference_resize_rgb24(
        src: &[u8],
        src_width: u32,
        src_height: u32,
        dst: &mut [u8],
        dst_width: u32,
        dst_height: u32,
    ) {
        if src_width == dst_width && src_height == dst_height {
            dst.copy_from_slice(src);
            return;
        }

        let x_scale = f64::from(src_width) / f64::from(dst_width.max(1));
        let y_scale = f64::from(src_height) / f64::from(dst_height.max(1));
        let src_width = src_width as usize;
        let src_height = src_height as usize;
        let dst_width = dst_width as usize;
        let dst_height = dst_height as usize;

        for y in 0..dst_height {
            let src_y = ((y as f64 + 0.5) * y_scale - 0.5).clamp(0.0, (src_height - 1) as f64);
            let y0 = src_y.floor() as usize;
            let y1 = (y0 + 1).min(src_height - 1);
            let y_weight = src_y - y0 as f64;
            for x in 0..dst_width {
                let src_x = ((x as f64 + 0.5) * x_scale - 0.5).clamp(0.0, (src_width - 1) as f64);
                let x0 = src_x.floor() as usize;
                let x1 = (x0 + 1).min(src_width - 1);
                let x_weight = src_x - x0 as f64;
                let dst_offset = (y * dst_width + x) * RAW_RGB_BYTES_PER_PIXEL;

                for channel in 0..RAW_RGB_BYTES_PER_PIXEL {
                    let top_left =
                        src[(y0 * src_width + x0) * RAW_RGB_BYTES_PER_PIXEL + channel] as f64;
                    let top_right =
                        src[(y0 * src_width + x1) * RAW_RGB_BYTES_PER_PIXEL + channel] as f64;
                    let bottom_left =
                        src[(y1 * src_width + x0) * RAW_RGB_BYTES_PER_PIXEL + channel] as f64;
                    let bottom_right =
                        src[(y1 * src_width + x1) * RAW_RGB_BYTES_PER_PIXEL + channel] as f64;
                    let top = top_left + (top_right - top_left) * x_weight;
                    let bottom = bottom_left + (bottom_right - bottom_left) * x_weight;
                    dst[dst_offset + channel] = (top + (bottom - top) * y_weight).round() as u8;
                }
            }
        }
    }
}
