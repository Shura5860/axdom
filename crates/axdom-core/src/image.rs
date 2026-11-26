use std::io::Cursor;

use serde::{Deserialize, Serialize};

use crate::error::{AxdomError, Result};
use crate::model::Bounds;

/// One labeled cell on a Set-of-Mark (SoM) coordinate grid overlaid onto a canvas/blind-spot screenshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SomCell {
    pub label: String,
    pub center_x: i32,
    pub center_y: i32,
}

/// Minimal RFC 4648 base64 decoder (mirrors `encode_base64`) for saving screenshots to disk.
pub fn decode_base64(input: &str) -> Result<Vec<u8>> {
    let bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !bytes.len().is_multiple_of(4) {
        return Err(AxdomError::Backend("invalid base64 length".into()));
    }
    let decode_char = |b: u8| -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    for chunk in bytes.as_chunks::<4>().0 {
        let a =
            decode_char(chunk[0]).ok_or_else(|| AxdomError::Backend("invalid base64".into()))?;
        let b =
            decode_char(chunk[1]).ok_or_else(|| AxdomError::Backend("invalid base64".into()))?;
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            let c = decode_char(chunk[2])
                .ok_or_else(|| AxdomError::Backend("invalid base64".into()))?;
            out.push((b << 4) | (c >> 2));
            if chunk[3] != b'=' {
                let d = decode_char(chunk[3])
                    .ok_or_else(|| AxdomError::Backend("invalid base64".into()))?;
                out.push((c << 6) | d);
            }
        }
    }
    Ok(out)
}

/// Crops an encoded PNG image to the given screen `bounds` (clamped to image dimensions).
/// Returns the original bytes untouched if `bounds` is zero-sized or covers the entire image.
pub fn crop_png_bytes(png_bytes: &[u8], bounds: Bounds) -> Result<Vec<u8>> {
    if bounds.is_zero_sized() {
        return Ok(png_bytes.to_vec());
    }

    let mut decoder = png::Decoder::new(Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| AxdomError::ActionFailed(format!("decode PNG for crop: {e}")))?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| AxdomError::ActionFailed(format!("read PNG frame for crop: {e}")))?;
    let src = &buf[..info.buffer_size()];

    let img_w = info.width;
    let img_h = info.height;
    if img_w == 0 || img_h == 0 {
        return Ok(png_bytes.to_vec());
    }

    let x0 = bounds.x.max(0) as u32;
    let y0 = bounds.y.max(0) as u32;
    if x0 >= img_w || y0 >= img_h {
        return Ok(png_bytes.to_vec());
    }
    let crop_w = (bounds.width.max(1) as u32).min(img_w - x0);
    let crop_h = (bounds.height.max(1) as u32).min(img_h - y0);
    if x0 == 0 && y0 == 0 && crop_w == img_w && crop_h == img_h {
        return Ok(png_bytes.to_vec());
    }

    let bpp = info.color_type.samples();
    let src_stride = (img_w as usize) * bpp;
    let dst_stride = (crop_w as usize) * bpp;
    let mut cropped = Vec::with_capacity(dst_stride * (crop_h as usize));

    for row in y0..(y0 + crop_h) {
        let start = (row as usize) * src_stride + (x0 as usize) * bpp;
        let end = start + dst_stride;
        if end <= src.len() {
            cropped.extend_from_slice(&src[start..end]);
        }
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, crop_w, crop_h);
        encoder.set_color(info.color_type);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| AxdomError::ActionFailed(format!("encode cropped PNG header: {e}")))?;
        writer
            .write_image_data(&cropped)
            .map_err(|e| AxdomError::ActionFailed(format!("encode cropped PNG data: {e}")))?;
    }

    Ok(out)
}

/// Draws a Set-of-Mark (SoM) coordinate grid (`A1`, `B1`, ...) onto a PNG image for canvas /
/// blind-spot apps that expose no accessibility tree, returning the annotated PNG bytes and
/// the real screen center coordinates of each cell (`origin_x/y` offsets for cropped regions).
pub fn annotate_som_grid(
    png_bytes: &[u8],
    origin_x: i32,
    origin_y: i32,
    cols: u32,
    rows: u32,
) -> Result<(Vec<u8>, Vec<SomCell>)> {
    let cols = cols.clamp(1, 6);
    let rows = rows.clamp(1, 9);

    let mut decoder = png::Decoder::new(Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|e| AxdomError::ActionFailed(format!("decode PNG for SoM grid: {e}")))?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| AxdomError::ActionFailed(format!("read PNG frame for SoM grid: {e}")))?;
    let w = info.width;
    let h = info.height;
    if w == 0 || h == 0 {
        return Ok((png_bytes.to_vec(), Vec::new()));
    }

    // Normalize into RGBA for clean grid and label rendering.
    let src = &buf[..info.buffer_size()];
    let mut rgba = vec![255u8; (w as usize) * (h as usize) * 4];
    match info.color_type {
        png::ColorType::Rgba => rgba.copy_from_slice(src),
        png::ColorType::Rgb => {
            for (i, px) in src.as_chunks::<3>().0.iter().enumerate() {
                rgba[i * 4] = px[0];
                rgba[i * 4 + 1] = px[1];
                rgba[i * 4 + 2] = px[2];
                rgba[i * 4 + 3] = 255;
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for (i, px) in src.as_chunks::<2>().0.iter().enumerate() {
                rgba[i * 4] = px[0];
                rgba[i * 4 + 1] = px[0];
                rgba[i * 4 + 2] = px[0];
                rgba[i * 4 + 3] = px[1];
            }
        }
        png::ColorType::Grayscale => {
            for (i, &g) in src.iter().enumerate() {
                rgba[i * 4] = g;
                rgba[i * 4 + 1] = g;
                rgba[i * 4 + 2] = g;
                rgba[i * 4 + 3] = 255;
            }
        }
        _ => {}
    }

    let blend_pixel = |rgba: &mut [u8], x: u32, y: u32, r: u8, g: u8, b: u8, alpha: u8| {
        if x >= w || y >= h {
            return;
        }
        let idx = ((y as usize) * (w as usize) + (x as usize)) * 4;
        let a = alpha as u32;
        let inv = 255 - a;
        rgba[idx] = ((r as u32 * a + rgba[idx] as u32 * inv) / 255) as u8;
        rgba[idx + 1] = ((g as u32 * a + rgba[idx + 1] as u32 * inv) / 255) as u8;
        rgba[idx + 2] = ((b as u32 * a + rgba[idx + 2] as u32 * inv) / 255) as u8;
        rgba[idx + 3] = 255;
    };

    // Draw semi-transparent red grid lines.
    for c in 1..cols {
        let gx = (w * c) / cols;
        for y in 0..h {
            blend_pixel(&mut rgba, gx, y, 255, 64, 64, 150);
        }
    }
    for r in 1..rows {
        let gy = (h * r) / rows;
        for x in 0..w {
            blend_pixel(&mut rgba, x, gy, 255, 64, 64, 150);
        }
    }

    let mut cells = Vec::with_capacity((cols * rows) as usize);
    for r in 0..rows {
        let y0 = (h * r) / rows;
        let y1 = (h * (r + 1)) / rows;
        for c in 0..cols {
            let x0 = (w * c) / cols;
            let x1 = (w * (c + 1)) / cols;
            let col_ch = (b'A' + c as u8) as char;
            let row_ch = (b'1' + r as u8) as char;
            let label = format!("{col_ch}{row_ch}");

            // Dark badge + bright yellow 3x5 bitmap glyphs (scaled 2x) in top-left of cell.
            let bx = x0 + 3;
            let by = y0 + 3;
            for py in by..by.saturating_add(14).min(y1) {
                for px in bx..bx.saturating_add(18).min(x1) {
                    blend_pixel(&mut rgba, px, py, 16, 16, 20, 210);
                }
            }
            draw_glyph(&mut rgba, w, h, bx + 2, by + 2, col_ch);
            draw_glyph(&mut rgba, w, h, bx + 10, by + 2, row_ch);

            cells.push(SomCell {
                label,
                center_x: origin_x + ((x0 + x1) / 2) as i32,
                center_y: origin_y + ((y0 + y1) / 2) as i32,
            });
        }
    }

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| AxdomError::ActionFailed(format!("encode SoM PNG header: {e}")))?;
        writer
            .write_image_data(&rgba)
            .map_err(|e| AxdomError::ActionFailed(format!("encode SoM PNG data: {e}")))?;
    }

    Ok((out, cells))
}

fn draw_glyph(rgba: &mut [u8], w: u32, h: u32, ox: u32, oy: u32, ch: char) {
    let bits: [u8; 5] = match ch {
        'A' => [0b010, 0b101, 0b111, 0b101, 0b101],
        'B' => [0b110, 0b101, 0b110, 0b101, 0b110],
        'C' => [0b011, 0b100, 0b100, 0b100, 0b011],
        'D' => [0b110, 0b101, 0b101, 0b101, 0b110],
        'E' => [0b111, 0b100, 0b110, 0b100, 0b111],
        'F' => [0b111, 0b100, 0b110, 0b100, 0b100],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b110, 0b001, 0b010, 0b100, 0b111],
        '3' => [0b110, 0b001, 0b010, 0b001, 0b110],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b110, 0b001, 0b110],
        '6' => [0b011, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b010, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b110],
        _ => [0b111, 0b101, 0b101, 0b101, 0b111],
    };
    for (row, &mask) in bits.iter().enumerate() {
        for col in 0..3u32 {
            if (mask >> (2 - col)) & 1 == 1 {
                for dy in 0..2u32 {
                    for dx in 0..2u32 {
                        let px = ox + col * 2 + dx;
                        let py = oy + (row as u32) * 2 + dy;
                        if px < w && py < h {
                            let idx = ((py as usize) * (w as usize) + (px as usize)) * 4;
                            rgba[idx] = 255;
                            rgba[idx + 1] = 230;
                            rgba[idx + 2] = 64;
                            rgba[idx + 3] = 255;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rgba_png(w: u32, h: u32) -> Vec<u8> {
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let idx = ((y * w + x) * 4) as usize;
                pixels[idx] = x as u8;
                pixels[idx + 1] = y as u8;
                pixels[idx + 2] = 200;
                pixels[idx + 3] = 255;
            }
        }
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&pixels).unwrap();
        }
        out
    }

    #[test]
    fn crops_png_to_clamped_bounds_accurately() {
        let full = make_rgba_png(10, 8);
        let cropped = crop_png_bytes(
            &full,
            Bounds {
                x: 2,
                y: 3,
                width: 4,
                height: 3,
            },
        )
        .unwrap();

        let decoder = png::Decoder::new(Cursor::new(&cropped));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(info.width, 4);
        assert_eq!(info.height, 3);
        // Top-left pixel of crop should be original pixel (2, 3).
        assert_eq!(&buf[0..4], &[2, 3, 200, 255]);
    }

    #[test]
    fn annotates_som_grid_with_expected_cell_centers() {
        let full = make_rgba_png(100, 80);
        let (annotated, cells) = annotate_som_grid(&full, 10, 20, 4, 4).unwrap();
        assert!(!annotated.is_empty());
        assert_eq!(cells.len(), 16);
        assert_eq!(cells[0].label, "A1");
        assert_eq!(cells[0].center_x, 10 + 12);
        assert_eq!(cells[0].center_y, 20 + 10);
        assert_eq!(cells[15].label, "D4");
    }
}
