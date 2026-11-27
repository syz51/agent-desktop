use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use desktop_proto::{ImageFormat, ScreenshotTarget};

use crate::backend::RawScreenshot;

const MAX_FRAMES_PER_TARGET: usize = 4;
const MAX_TRACKED_TARGETS: usize = 16;
/// Maximum age for serving a cached unannotated frame when no mutating action has occurred.
pub const DEFAULT_WARM_FRAME_TTL_MS: i64 = 120;
/// Minimum per-tile luma delta (0..=255) on the 8x8 area-averaged thumbnail to count as a real
/// visual change (ignores 1-4 LSB JPEG compression jitter while catching small UI updates).
pub const DEFAULT_VISUAL_DIFF_THRESHOLD: u8 = 6;

/// Normalized cache key for a screenshot target.
///
/// Keeps `None` (main display) distinct from `Some(0)` so multi-monitor hosts where the main
/// display is not at index 0 never alias different monitors.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FrameKey {
    Display(Option<u32>),
    Window(u32),
    App(String),
    Region {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        display: Option<u32>,
    },
}

impl FrameKey {
    #[must_use]
    pub fn from_target(target: &ScreenshotTarget) -> Self {
        match target {
            ScreenshotTarget::Display { index } => Self::Display(*index),
            ScreenshotTarget::Window { window_id } => Self::Window(*window_id),
            ScreenshotTarget::App { app } => Self::App(app.trim().to_ascii_lowercase()),
            ScreenshotTarget::Region {
                x,
                y,
                width,
                height,
                display,
            } => Self::Region {
                x: x.round() as i32,
                y: y.round() as i32,
                w: width.round().max(1.0) as u32,
                h: height.round().max(1.0) as u32,
                display: *display,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedFrame {
    pub captured_at: DateTime<Utc>,
    pub format: ImageFormat,
    pub raw: RawScreenshot,
    pub hash: String,
    pub visual_sig: Option<[u8; 64]>,
}

/// Bounded per-target ring buffer of recent captured frames driven by the injectable `Clock`.
#[derive(Default)]
pub struct FrameRing {
    entries: HashMap<FrameKey, VecDeque<CachedFrame>>,
    last_invalidation_at: Option<DateTime<Utc>>,
    hits: u64,
    misses: u64,
}

impl FrameRing {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a freshly captured frame at timestamp `now` into the ring buffer.
    pub fn record(
        &mut self,
        now: DateTime<Utc>,
        key: FrameKey,
        format: ImageFormat,
        raw: RawScreenshot,
        hash: String,
    ) {
        if !self.entries.contains_key(&key)
            && self.entries.len() >= MAX_TRACKED_TARGETS
            && let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, q)| q.back().map(|f| f.captured_at))
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest_key);
        }
        let visual_sig = raw.visual_sig;
        let ring = self.entries.entry(key).or_default();
        if ring.len() >= MAX_FRAMES_PER_TARGET {
            ring.pop_front();
        }
        ring.push_back(CachedFrame {
            captured_at: now,
            format,
            raw,
            hash,
            visual_sig,
        });
    }

    /// Return a cached frame if it matches `format`, is at most `max_age_ms` old relative to
    /// `now`, and was captured after the most recent invalidation.
    pub fn get_fresh(
        &mut self,
        now: DateTime<Utc>,
        key: &FrameKey,
        format: ImageFormat,
        max_age_ms: i64,
    ) -> Option<CachedFrame> {
        let max_age = ChronoDuration::milliseconds(max_age_ms);
        let found = self.entries.get(key).and_then(|q| {
            q.iter()
                .rev()
                .find(|f| {
                    let after_invalidation = self
                        .last_invalidation_at
                        .is_none_or(|inv| f.captured_at >= inv);
                    let age = now.signed_duration_since(f.captured_at);
                    f.format == format
                        && after_invalidation
                        && age >= ChronoDuration::zero()
                        && age <= max_age
                })
                .cloned()
        });
        if found.is_some() {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        found
    }

    /// Mark all cached frames stale after a UI action, wait, or sleep.
    pub fn invalidate_all(&mut self, now: DateTime<Utc>) {
        self.last_invalidation_at = Some(now);
        self.entries.clear();
    }

    #[must_use]
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}

/// Compute an 8x8 area-averaged luma thumbnail (`[u8; 64]`) directly from RGBA8 or BGRA8 pixels
/// in zero heap allocations.
///
/// Samples up to 8x8 sub-points inside each of the 64 tiles so the entire image area is covered
/// uniformly (preventing both blind spots on small UI widgets and cross-backend filter mismatch).
#[must_use]
pub fn visual_signature_from_pixels(
    pixels: &[u8],
    width: usize,
    height: usize,
    bytes_per_row: usize,
    is_bgra: bool,
) -> Option<[u8; 64]> {
    if width < 8 || height < 8 || bytes_per_row < width * 4 || pixels.len() < height * bytes_per_row
    {
        return None;
    }

    let mut sig = [0u8; 64];
    for ty in 0..8usize {
        let y0 = (ty * height) / 8;
        let y1 = (((ty + 1) * height) / 8).max(y0 + 1).min(height);
        let y_step = ((y1 - y0) / 8).max(1);
        for tx in 0..8usize {
            let x0 = (tx * width) / 8;
            let x1 = (((tx + 1) * width) / 8).max(x0 + 1).min(width);
            let x_step = ((x1 - x0) / 8).max(1);

            let mut sum = 0u64;
            let mut count = 0u64;
            let mut y = y0;
            while y < y1 {
                let row_off = y * bytes_per_row;
                let mut x = x0;
                while x < x1 {
                    let off = row_off + x * 4;
                    let (r, g, b) = if is_bgra {
                        (
                            u64::from(pixels[off + 2]),
                            u64::from(pixels[off + 1]),
                            u64::from(pixels[off]),
                        )
                    } else {
                        (
                            u64::from(pixels[off]),
                            u64::from(pixels[off + 1]),
                            u64::from(pixels[off + 2]),
                        )
                    };
                    // BT.601 luma scaled to 0..255: (77*R + 150*G + 29*B) >> 8.
                    sum += (77 * r + 150 * g + 29 * b) >> 8;
                    count += 1;
                    x += x_step;
                }
                y += y_step;
            }
            sig[ty * 8 + tx] = (sum / count.max(1)).min(255) as u8;
        }
    }
    Some(sig)
}

/// Maximum per-tile luma difference between two 8x8 visual signatures.
#[must_use]
pub fn max_visual_delta(a: &[u8; 64], b: &[u8; 64]) -> u8 {
    a.iter()
        .zip(b.iter())
        .map(|(&x, &y)| x.abs_diff(y))
        .max()
        .unwrap_or(0)
}

/// Determine whether a frame represents a real visual change vs a baseline frame.
///
/// Compares the 8x8 area-averaged luma thumbnail (`max_visual_delta >= 6`) when both signatures
/// exist to filter out JPEG compression noise while catching localized UI updates; falls back to
/// exact content SHA-256 when signatures are absent.
#[must_use]
pub fn has_visual_change(
    base_sha: &str,
    base_sig: Option<&[u8; 64]>,
    curr_sha: &str,
    curr_sig: Option<&[u8; 64]>,
) -> bool {
    if base_sha == curr_sha {
        return false;
    }
    if let (Some(a), Some(b)) = (base_sig, curr_sig) {
        return max_visual_delta(a, b) >= DEFAULT_VISUAL_DIFF_THRESHOLD;
    }
    true
}

#[cfg(test)]
mod tests {
    use desktop_proto::Rect;

    use super::*;

    fn sample_raw(bytes: Vec<u8>, visual_sig: Option<[u8; 64]>) -> RawScreenshot {
        RawScreenshot {
            width: 100,
            height: 80,
            display_bounds: Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 80.0,
            },
            mime_type: "image/jpeg".into(),
            bytes,
            visual_sig,
        }
    }

    #[test]
    fn frame_ring_respects_injectable_clock_ttl_keys_and_invalidation() {
        let mut ring = FrameRing::new();
        let t0 = DateTime::parse_from_rfc3339("2026-11-27T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let main_key = FrameKey::from_target(&ScreenshotTarget::Display { index: None });
        let idx0_key = FrameKey::from_target(&ScreenshotTarget::Display { index: Some(0) });
        assert_ne!(
            main_key, idx0_key,
            "main display None must not alias index Some(0)"
        );

        ring.record(
            t0,
            main_key.clone(),
            ImageFormat::Jpeg,
            sample_raw(vec![1, 2, 3], Some([42u8; 64])),
            "hash1".into(),
        );

        // Fresh hit at +50 ms.
        let t50 = t0 + ChronoDuration::milliseconds(50);
        let hit = ring
            .get_fresh(t50, &main_key, ImageFormat::Jpeg, DEFAULT_WARM_FRAME_TTL_MS)
            .expect("warm hit");
        assert_eq!(hit.hash, "hash1");

        // Format mismatch and distinct display index must miss.
        assert!(
            ring.get_fresh(t50, &main_key, ImageFormat::Png, DEFAULT_WARM_FRAME_TTL_MS)
                .is_none()
        );
        assert!(
            ring.get_fresh(t50, &idx0_key, ImageFormat::Jpeg, DEFAULT_WARM_FRAME_TTL_MS)
                .is_none()
        );

        // Expired at +200 ms (> 120 ms TTL).
        let t200 = t0 + ChronoDuration::milliseconds(200);
        assert!(
            ring.get_fresh(
                t200,
                &main_key,
                ImageFormat::Jpeg,
                DEFAULT_WARM_FRAME_TTL_MS
            )
            .is_none()
        );

        // Invalidation clears immediately.
        ring.invalidate_all(t50);
        assert!(
            ring.get_fresh(t50, &main_key, ImageFormat::Jpeg, DEFAULT_WARM_FRAME_TTL_MS)
                .is_none()
        );
    }

    #[test]
    fn visual_signature_catches_localized_ui_change_and_ignores_jitter() {
        let w = 64usize;
        let h = 64usize;
        let base_px = [100u8, 100, 100, 255].repeat(w * h);
        let jitter_px = [102u8, 101, 100, 255].repeat(w * h);
        let mut toast_px = base_px.clone();
        // Paint a localized bright white toast inside a single 8x8 tile (top-right corner).
        for y in 0..8 {
            for x in 56..64 {
                let off = (y * w + x) * 4;
                toast_px[off..off + 4].copy_from_slice(&[240, 240, 240, 255]);
            }
        }

        let s_base = visual_signature_from_pixels(&base_px, w, h, w * 4, false).unwrap();
        let s_jitter = visual_signature_from_pixels(&jitter_px, w, h, w * 4, false).unwrap();
        let s_toast = visual_signature_from_pixels(&toast_px, w, h, w * 4, false).unwrap();

        assert!(!has_visual_change("a", Some(&s_base), "b", Some(&s_jitter)));
        assert!(has_visual_change("a", Some(&s_base), "c", Some(&s_toast)));
        assert!(!has_visual_change("same", None, "same", None));
    }
}
