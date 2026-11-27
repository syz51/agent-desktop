use std::io::Cursor;

use desktop_core::{AnnotateBox, RawScreenshot, draw_annotations_rgba};
use desktop_proto::{ErrorCode, ImageFormat, Rect, RpcError, fit_screenshot};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::{ColorType, ImageEncoder as _, RgbaImage};

/// Pixel layout parameters for an X11 `ZPixmap` buffer.
#[derive(Debug, Clone, Copy)]
pub struct ZPixmapFormat {
    pub width: u32,
    pub height: u32,
    pub bits_per_pixel: u8,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub msb_first: bool,
}

/// Convert an X11 `ZPixmap` pixel buffer (24-bit or 32-bit true color) into tight RGBA8 bytes.
pub fn zpixmap_to_rgba(raw: &[u8], fmt: ZPixmapFormat) -> Result<Vec<u8>, RpcError> {
    let bpp = usize::from(fmt.bits_per_pixel / 8);
    if bpp != 3 && bpp != 4 {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            format!(
                "unsupported X11 ZPixmap bits_per_pixel={}",
                fmt.bits_per_pixel
            ),
        ));
    }
    let px_count = (fmt.width as usize).saturating_mul(fmt.height as usize);
    let expected = px_count.saturating_mul(bpp);
    if raw.len() < expected {
        return Err(RpcError::new(
            ErrorCode::BackendError,
            format!(
                "short X11 image buffer: got {} bytes, expected at least {expected}",
                raw.len()
            ),
        ));
    }

    // Default to standard X11 24/32-bit BGRX masks if visual masks were not provided.
    let (rm, gm, bm) = if fmt.red_mask == 0 && fmt.green_mask == 0 && fmt.blue_mask == 0 {
        (0x00ff_0000, 0x0000_ff00, 0x0000_00ff)
    } else {
        (fmt.red_mask, fmt.green_mask, fmt.blue_mask)
    };
    let r_shift = rm.trailing_zeros();
    let g_shift = gm.trailing_zeros();
    let b_shift = bm.trailing_zeros();

    let mut out = Vec::with_capacity(px_count * 4);
    for chunk in raw[..expected].chunks_exact(bpp) {
        let pixel: u32 = match (bpp, fmt.msb_first) {
            (4, false) => u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            (4, true) => u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            (3, false) => u32::from_le_bytes([chunk[0], chunk[1], chunk[2], 0]),
            (3, true) => u32::from_be_bytes([0, chunk[0], chunk[1], chunk[2]]),
            _ => unreachable!(),
        };
        let r = ((pixel & rm) >> r_shift) as u8;
        let g = ((pixel & gm) >> g_shift) as u8;
        let b = ((pixel & bm) >> b_shift) as u8;
        out.extend_from_slice(&[r, g, b, 255]);
    }
    Ok(out)
}

/// Resize (if above the 1280px cap), draw Set-of-Marks annotations, and encode JPEG/PNG in memory.
pub fn encode_rgba_screenshot(
    rgba: Vec<u8>,
    src_w: u32,
    src_h: u32,
    display_bounds: Rect,
    format: ImageFormat,
    annotate: &[AnnotateBox],
) -> Result<RawScreenshot, RpcError> {
    let img = RgbaImage::from_raw(src_w, src_h, rgba).ok_or_else(|| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("invalid RGBA buffer for {src_w}x{src_h}"),
        )
    })?;

    let (out_w, out_h) = fit_screenshot(src_w, src_h);
    let mut final_img = if out_w != src_w || out_h != src_h {
        image::imageops::resize(&img, out_w, out_h, FilterType::Triangle)
    } else {
        img
    };

    let dhash = desktop_core::dhash_from_pixels(
        final_img.as_raw(),
        out_w as usize,
        out_h as usize,
        out_w as usize * 4,
        false,
    );

    if !annotate.is_empty() {
        let w = final_img.width() as usize;
        let h = final_img.height() as usize;
        draw_annotations_rgba(final_img.as_mut(), w, h, w * 4, display_bounds, annotate);
    }

    let bytes = match format {
        ImageFormat::Jpeg => {
            let rgb = image::DynamicImage::ImageRgba8(final_img).to_rgb8();
            let mut buf = Cursor::new(Vec::new());
            let mut enc = JpegEncoder::new_with_quality(&mut buf, 82);
            enc.encode(rgb.as_raw(), out_w, out_h, ColorType::Rgb8.into())
                .map_err(|e| {
                    RpcError::new(ErrorCode::BackendError, format!("JPEG encode failed: {e}"))
                })?;
            buf.into_inner()
        }
        ImageFormat::Png => {
            let mut buf = Cursor::new(Vec::new());
            let enc = PngEncoder::new(&mut buf);
            enc.write_image(final_img.as_raw(), out_w, out_h, ColorType::Rgba8.into())
                .map_err(|e| {
                    RpcError::new(ErrorCode::BackendError, format!("PNG encode failed: {e}"))
                })?;
            buf.into_inner()
        }
    };

    Ok(RawScreenshot {
        width: out_w,
        height: out_h,
        display_bounds,
        mime_type: format.mime_type().to_string(),
        bytes,
        dhash,
    })
}

/// Decode encoded PNG/JPEG bytes (e.g. from Wayland `grim` or XDG Screenshot portal) into RGBA8.
pub fn decode_to_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), RpcError> {
    let dyn_img = image::load_from_memory(bytes).map_err(|e| {
        RpcError::new(
            ErrorCode::BackendError,
            format!("failed to decode captured image: {e}"),
        )
    })?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((rgba.into_raw(), w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_bgrx_zpixmap_and_encodes_jpeg_png_with_annotations() {
        // 4x2 little-endian BGRX pixels: B=10, G=20, R=30, X=0
        let mut raw = Vec::new();
        for _ in 0..8 {
            raw.extend_from_slice(&[10, 20, 30, 0]);
        }
        let rgba = zpixmap_to_rgba(
            &raw,
            ZPixmapFormat {
                width: 4,
                height: 2,
                bits_per_pixel: 32,
                red_mask: 0x00ff_0000,
                green_mask: 0x0000_ff00,
                blue_mask: 0x0000_00ff,
                msb_first: false,
            },
        )
        .unwrap();
        assert_eq!(&rgba[..4], &[30, 20, 10, 255]);

        // Test JPEG + PNG + annotation round-trip on a 100x80 image.
        let solid = [20u8, 40, 80, 255].repeat(100 * 80);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 80.0,
        };
        let jpeg =
            encode_rgba_screenshot(solid.clone(), 100, 80, bounds, ImageFormat::Jpeg, &[]).unwrap();
        assert_eq!(&jpeg.bytes[..3], &[0xFF, 0xD8, 0xFF]);

        let plain_png =
            encode_rgba_screenshot(solid.clone(), 100, 80, bounds, ImageFormat::Png, &[]).unwrap();
        assert_eq!(
            &plain_png.bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );

        let ann_png = encode_rgba_screenshot(
            solid,
            100,
            80,
            bounds,
            ImageFormat::Png,
            &[AnnotateBox {
                ref_id: "@e1".into(),
                bounds: Rect {
                    x: 10.0,
                    y: 12.0,
                    width: 30.0,
                    height: 20.0,
                },
            }],
        )
        .unwrap();
        assert_ne!(ann_png.bytes, plain_png.bytes);
    }
}
