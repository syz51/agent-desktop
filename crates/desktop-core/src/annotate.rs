use desktop_proto::Rect;

use crate::backend::AnnotateBox;

/// Draw crisp Set-of-Marks bounding boxes and `@eN` tags directly onto an RGBA8 pixel buffer.
///
/// Shared by both macOS (`desktop-macos`) and Linux (`desktop-linux`) screenshot pipelines.
pub fn draw_annotations_rgba(
    pixels: &mut [u8],
    img_w: usize,
    img_h: usize,
    bpr: usize,
    display_bounds: Rect,
    boxes: &[AnnotateBox],
) {
    if display_bounds.is_empty() || img_w == 0 || img_h == 0 || bpr < img_w * 4 {
        return;
    }

    let sx = img_w as f64 / display_bounds.width;
    let sy = img_h as f64 / display_bounds.height;

    for b in boxes {
        let x0 = ((b.bounds.x - display_bounds.x) * sx).round() as isize;
        let y0 = ((b.bounds.y - display_bounds.y) * sy).round() as isize;
        let x1 = ((b.bounds.x + b.bounds.width - display_bounds.x) * sx).round() as isize;
        let y1 = ((b.bounds.y + b.bounds.height - display_bounds.y) * sy).round() as isize;
        if x1 <= x0 || y1 <= y0 || x1 < 0 || y1 < 0 || x0 >= img_w as isize || y0 >= img_h as isize
        {
            continue;
        }
        draw_rect_outline(
            pixels,
            img_w,
            img_h,
            bpr,
            x0,
            y0,
            x1,
            y1,
            [255, 68, 58, 255],
        );
        draw_badge(pixels, img_w, img_h, bpr, x0, y0, &b.ref_id);
    }
}

#[allow(clippy::too_many_arguments)]
fn set_px(pixels: &mut [u8], w: usize, h: usize, bpr: usize, x: isize, y: isize, rgba: [u8; 4]) {
    if x < 0 || y < 0 || x >= w as isize || y >= h as isize {
        return;
    }
    let off = (y as usize) * bpr + (x as usize) * 4;
    if off + 4 <= pixels.len() {
        pixels[off..off + 4].copy_from_slice(&rgba);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_rect_outline(
    pixels: &mut [u8],
    w: usize,
    h: usize,
    bpr: usize,
    x0: isize,
    y0: isize,
    x1: isize,
    y1: isize,
    rgba: [u8; 4],
) {
    for t in 0..2 {
        for x in x0..=x1 {
            set_px(pixels, w, h, bpr, x, y0 + t, rgba);
            set_px(pixels, w, h, bpr, x, y1 - t, rgba);
        }
        for y in y0..=y1 {
            set_px(pixels, w, h, bpr, x0 + t, y, rgba);
            set_px(pixels, w, h, bpr, x1 - t, y, rgba);
        }
    }
}

fn draw_badge(
    pixels: &mut [u8],
    w: usize,
    h: usize,
    bpr: usize,
    x0: isize,
    y0: isize,
    label: &str,
) {
    let short = label.trim_start_matches('@');
    let bw = (short.len() as isize) * 6 + 6;
    let bh = 11isize;
    let bx = x0.max(0);
    let by = (y0 - bh).max(0);
    for dy in 0..bh {
        for dx in 0..bw {
            set_px(pixels, w, h, bpr, bx + dx, by + dy, [220, 38, 38, 255]);
        }
    }
    for (ci, ch) in short.bytes().enumerate() {
        let glyph = glyph_5x7(ch);
        let gx = bx + 3 + (ci as isize) * 6;
        let gy = by + 2;
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if (bits >> (4 - col)) & 1 != 0 {
                    set_px(
                        pixels,
                        w,
                        h,
                        bpr,
                        gx + col as isize,
                        gy + row as isize,
                        [255, 255, 255, 255],
                    );
                }
            }
        }
    }
}

/// Minimal 5x7 bitmap font for `e` and digits `0`-`9` used in Set-of-Marks badges.
fn glyph_5x7(ch: u8) -> [u8; 7] {
    match ch {
        b'e' | b'E' => [
            0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110, 0b00000,
        ],
        b'0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        b'1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        b'2' => [
            0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
        ],
        b'3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        b'4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        b'5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        b'6' => [
            0b01110, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        b'7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        b'8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        b'9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        _ => [0; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_red_outline_and_badge_without_panicking_on_edge_boxes() {
        let w = 80;
        let h = 60;
        let mut pixels = vec![0u8; w * h * 4];
        draw_annotations_rgba(
            &mut pixels,
            w,
            h,
            w * 4,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 80.0,
                height: 60.0,
            },
            &[
                AnnotateBox {
                    ref_id: "@e1".into(),
                    bounds: Rect {
                        x: 10.0,
                        y: 15.0,
                        width: 25.0,
                        height: 15.0,
                    },
                },
                // Partially out-of-bounds box should clip safely.
                AnnotateBox {
                    ref_id: "@e2".into(),
                    bounds: Rect {
                        x: -5.0,
                        y: -5.0,
                        width: 20.0,
                        height: 12.0,
                    },
                },
            ],
        );
        // Top-left border pixel of @e1 at (10, 15) should be red [255, 68, 58, 255].
        let off = 15 * w * 4 + 10 * 4;
        assert_eq!(&pixels[off..off + 4], &[255, 68, 58, 255]);
    }
}
