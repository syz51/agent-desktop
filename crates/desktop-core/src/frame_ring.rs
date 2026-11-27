use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use desktop_proto::{ImageFormat, ScreenshotTarget};

use crate::backend::RawScreenshot;

const MAX_FRAMES_PER_TARGET: usize = 4;
const MAX_TRACKED_TARGETS: usize = 16;
/// Maximum age for serving a cached unannotated frame when no mutating action has occurred.
pub const DEFAULT_WARM_FRAME_TTL: Duration = Duration::from_millis(120);
/// Minimum 64-bit dHash Hamming distance treated as a real visual change (ignores 1-bit JPEG jitter).
pub const DEFAULT_VISUAL_DIFF_BITS: u32 = 2;

/// Normalized cache key for a screenshot target.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FrameKey {
    Display(u32),
    Window(u32),
    App(String),
    Region {
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        display: u32,
    },
}

impl FrameKey {
    #[must_use]
    pub fn from_target(target: &ScreenshotTarget) -> Self {
        match target {
            ScreenshotTarget::Display { index } => Self::Display(index.unwrap_or(0)),
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
                display: display.unwrap_or(0),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct CachedFrame {
    pub captured_at: Instant,
    pub format: ImageFormat,
    pub raw: RawScreenshot,
    pub hash: String,
    pub dhash: Option<u64>,
}

/// Bounded per-target ring buffer of recent captured frames with mutation-aware invalidation.
pub struct FrameRing {
    entries: HashMap<FrameKey, VecDeque<CachedFrame>>,
    last_mutation_at: Instant,
    hits: u64,
    misses: u64,
}

impl Default for FrameRing {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameRing {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            last_mutation_at: Instant::now(),
            hits: 0,
            misses: 0,
        }
    }

    /// Record a freshly captured frame into the ring buffer.
    pub fn record(&mut self, key: FrameKey, format: ImageFormat, raw: RawScreenshot, hash: String) {
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_TRACKED_TARGETS {
            // Evict the oldest target queue when the map hits its cap.
            if let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, q)| q.back().map(|f| f.captured_at))
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest_key);
            }
        }
        let dhash = raw.dhash;
        let ring = self.entries.entry(key).or_default();
        if ring.len() >= MAX_FRAMES_PER_TARGET {
            ring.pop_front();
        }
        ring.push_back(CachedFrame {
            captured_at: Instant::now(),
            format,
            raw,
            hash,
            dhash,
        });
    }

    /// Return a cached frame if it matches `format`, is at most `max_age` old, and was captured
    /// after the most recent mutating action.
    pub fn get_fresh(
        &mut self,
        key: &FrameKey,
        format: ImageFormat,
        max_age: Duration,
    ) -> Option<CachedFrame> {
        let now = Instant::now();
        let found = self.entries.get(key).and_then(|q| {
            q.iter()
                .rev()
                .find(|f| {
                    f.format == format
                        && f.captured_at >= self.last_mutation_at
                        && now.saturating_duration_since(f.captured_at) <= max_age
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

    /// Mark all cached frames stale after a mutating UI action (`click`, `key`, `set_text`, etc.).
    pub fn invalidate_all(&mut self) {
        self.last_mutation_at = Instant::now();
        self.entries.clear();
    }

    #[must_use]
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }
}

/// Compute a 64-bit perceptual difference hash (9x8 horizontal luma gradient) directly from RGBA8
/// (or BGRA8 with `is_bgra = true`) pixels in zero heap allocations.
#[must_use]
pub fn dhash_from_pixels(
    pixels: &[u8],
    width: usize,
    height: usize,
    bytes_per_row: usize,
    is_bgra: bool,
) -> Option<u64> {
    if width < 9 || height < 8 || bytes_per_row < width * 4 || pixels.len() < height * bytes_per_row
    {
        return None;
    }

    let mut luma = [[0u32; 9]; 8];
    for (row, luma_row) in luma.iter_mut().enumerate() {
        let y = (row * (height - 1)) / 7;
        let row_off = y * bytes_per_row;
        for (col, cell) in luma_row.iter_mut().enumerate() {
            let x = (col * (width - 1)) / 8;
            let off = row_off + x * 4;
            let (r, g, b) = if is_bgra {
                (
                    u32::from(pixels[off + 2]),
                    u32::from(pixels[off + 1]),
                    u32::from(pixels[off]),
                )
            } else {
                (
                    u32::from(pixels[off]),
                    u32::from(pixels[off + 1]),
                    u32::from(pixels[off + 2]),
                )
            };
            // Standard BT.601 integer luma approximation: 77*R + 150*G + 29*B.
            *cell = 77 * r + 150 * g + 29 * b;
        }
    }

    let mut bits = 0u64;
    let mut bit_idx = 0u32;
    for luma_row in &luma {
        for col in 0..8 {
            if luma_row[col] > luma_row[col + 1] {
                bits |= 1u64 << bit_idx;
            }
            bit_idx += 1;
        }
    }
    Some(bits)
}

/// Hamming distance between two 64-bit perceptual hashes.
#[must_use]
pub fn dhash_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Determine whether a frame represents a real visual change vs a baseline frame.
///
/// Uses perceptual `dhash` (distance >= `DEFAULT_VISUAL_DIFF_BITS`) when both hashes exist to
/// avoid false positives from JPEG compression noise, falling back to exact content SHA-256.
#[must_use]
pub fn has_visual_change(
    base_sha: &str,
    base_dhash: Option<u64>,
    curr_sha: &str,
    curr_dhash: Option<u64>,
) -> bool {
    if base_sha == curr_sha {
        return false;
    }
    if let (Some(a), Some(b)) = (base_dhash, curr_dhash) {
        return dhash_distance(a, b) >= DEFAULT_VISUAL_DIFF_BITS;
    }
    true
}

#[cfg(test)]
mod tests {
    use desktop_proto::Rect;

    use super::*;

    fn sample_raw(bytes: Vec<u8>, dhash: Option<u64>) -> RawScreenshot {
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
            dhash,
        }
    }

    #[test]
    fn frame_ring_serves_fresh_frames_and_invalidates_on_mutation() {
        let mut ring = FrameRing::new();
        let key = FrameKey::Display(0);
        assert!(
            ring.get_fresh(&key, ImageFormat::Jpeg, DEFAULT_WARM_FRAME_TTL)
                .is_none()
        );

        ring.record(
            key.clone(),
            ImageFormat::Jpeg,
            sample_raw(vec![1, 2, 3], Some(0x1234)),
            "hash1".into(),
        );
        let hit = ring
            .get_fresh(&key, ImageFormat::Jpeg, DEFAULT_WARM_FRAME_TTL)
            .expect("warm hit");
        assert_eq!(hit.hash, "hash1");
        assert_eq!(hit.dhash, Some(0x1234));
        // Format mismatch must miss.
        assert!(
            ring.get_fresh(&key, ImageFormat::Png, DEFAULT_WARM_FRAME_TTL)
                .is_none()
        );

        ring.invalidate_all();
        assert!(
            ring.get_fresh(&key, ImageFormat::Jpeg, DEFAULT_WARM_FRAME_TTL)
                .is_none()
        );
    }

    #[test]
    fn dhash_distinguishes_real_gradients_and_ignores_single_bit_jitter() {
        let w = 64usize;
        let h = 48usize;
        // Horizontal dark->bright gradient vs bright->dark gradient.
        let mut left_to_right = vec![0u8; w * h * 4];
        let mut right_to_left = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let off = (y * w + x) * 4;
                let v = ((x * 255) / (w - 1)) as u8;
                left_to_right[off..off + 4].copy_from_slice(&[v, v, v, 255]);
                let inv = 255 - v;
                right_to_left[off..off + 4].copy_from_slice(&[inv, inv, inv, 255]);
            }
        }
        let h1 = dhash_from_pixels(&left_to_right, w, h, w * 4, false).unwrap();
        let h2 = dhash_from_pixels(&right_to_left, w, h, w * 4, false).unwrap();
        assert_eq!(dhash_distance(h1, h2), 64);
        assert!(has_visual_change("a", Some(h1), "b", Some(h2)));
        // 1-bit dhash difference should be treated as noise, not a real visual change.
        assert!(!has_visual_change("a", Some(h1), "b", Some(h1 ^ 1)));
        // Exact same SHA is always unchanged.
        assert!(!has_visual_change("same", None, "same", None));
    }
}
