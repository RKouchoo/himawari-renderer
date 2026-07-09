//! Cold-cloud centroid tracking for `--follow-storm`.
//!
//! A tropical cyclone is, to B13, a large contiguous blob of very cold
//! cloud. The tracker seeds on the coldest large-scale blob on the disk
//! (away from the limb and the poles), then follows it frame to frame with
//! a bounded search window and a weighted centroid — no machine learning,
//! no external data, and robust to the eye clouding over. Thresholds live
//! in `tuning.rs`.

use crate::compose::FULL_DISK_SIZE;
use crate::tuning::*;

/// A tracked position on the 1 km composite grid, in pixels.
pub type Position = (f64, f64);

/// Advance the track by one frame: seed if this is the first fix, otherwise
/// search near the previous position. None only when seeding fails (no
/// storm-like cloud anywhere usable on the disk).
pub fn update(bt: &[f32], width: usize, previous: Option<Position>) -> Option<Position> {
    match previous {
        Some(prev) => Some(follow(bt, width, prev)),
        None => seed(bt, width),
    }
}

/// Lock onto the cold-cloud mass nearest a user-provided seed: an
/// unsmoothed centroid fix (falling back to the seed itself), so the first
/// frame is already centered instead of easing in over several frames.
pub fn acquire(bt: &[f32], width: usize, seed: Position) -> Position {
    let scale = width as f64 / FULL_DISK_SIZE as f64;
    centroid(bt, width, (seed.0 * scale, seed.1 * scale)).unwrap_or(seed)
}

/// How storm-like one pixel is: kelvins below the cold threshold.
fn weight(bt: f32) -> f64 {
    if bt.is_finite() {
        f64::from((TRACK_COLD_THRESHOLD - bt).max(0.0))
    } else {
        0.0
    }
}

/// First fix: slide a TRACK_SEED_WINDOW box over the (coarsely sampled)
/// disk and pick the one holding the most cold-cloud weight, then refine to
/// its centroid. The limb and high latitudes are excluded — limb cooling
/// and the winter polar surface both masquerade as cold cloud.
fn seed(bt: &[f32], width: usize) -> Option<Position> {
    const STRIDE: usize = 8;
    let half = width as f64 / 2.0;
    let max_radius = half * TRACK_SEED_MAX_RADIUS;

    let coarse_width = width / STRIDE;
    let mut coarse = vec![0f64; coarse_width * coarse_width];
    for y in 0..coarse_width {
        for x in 0..coarse_width {
            let (gx, gy) = ((x * STRIDE) as f64, (y * STRIDE) as f64);
            if ((gx - half).powi(2) + (gy - half).powi(2)).sqrt() <= max_radius {
                coarse[y * coarse_width + x] = weight(bt[y * STRIDE * width + x * STRIDE]);
            }
        }
    }

    let window = TRACK_SEED_WINDOW / STRIDE;
    let mut best = (0f64, 0usize, 0usize);
    for y in (0..coarse_width.saturating_sub(window)).step_by(window / 4) {
        for x in (0..coarse_width.saturating_sub(window)).step_by(window / 4) {
            let total: f64 = (y..y + window)
                .map(|row| coarse[row * coarse_width + x..][..window].iter().sum::<f64>())
                .sum();
            if total > best.0 {
                best = (total, x, y);
            }
        }
    }
    if best.0 < TRACK_MIN_WEIGHT {
        return None;
    }

    // Refine: centroid at full resolution within the winning box.
    let center = (
        ((best.1 + window / 2) * STRIDE) as f64,
        ((best.2 + window / 2) * STRIDE) as f64,
    );
    Some(centroid(bt, width, center).unwrap_or(to_composite(center, width)))
}

/// Subsequent frames: weighted centroid within the search radius of the
/// previous fix, blended in with TRACK_SMOOTHING. Holds position when the
/// storm's signal drops out (dissipation, or a housekeeping gap upstream).
fn follow(bt: &[f32], width: usize, previous: Position) -> Position {
    let scale = width as f64 / FULL_DISK_SIZE as f64;
    let prev_bt = (previous.0 * scale, previous.1 * scale);
    match centroid(bt, width, prev_bt) {
        Some(fix) => (
            previous.0 + (fix.0 - previous.0) * TRACK_SMOOTHING,
            previous.1 + (fix.1 - previous.1) * TRACK_SMOOTHING,
        ),
        None => previous,
    }
}

/// Cold-weighted centroid within the search box around `center` (both in
/// B13 grid pixels), returned on the 1 km composite grid. None when the box
/// holds too little cold cloud to be a fix.
fn centroid(bt: &[f32], width: usize, center: (f64, f64)) -> Option<Position> {
    let x0 = (center.0 as isize - TRACK_SEARCH_RADIUS as isize).max(0) as usize;
    let y0 = (center.1 as isize - TRACK_SEARCH_RADIUS as isize).max(0) as usize;
    let x1 = (center.0 as usize + TRACK_SEARCH_RADIUS).min(width - 1);
    let y1 = (center.1 as usize + TRACK_SEARCH_RADIUS).min(width - 1);

    let (mut total, mut sum_x, mut sum_y) = (0f64, 0f64, 0f64);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let w = weight(bt[y * width + x]);
            total += w;
            sum_x += w * x as f64;
            sum_y += w * y as f64;
        }
    }
    if total < TRACK_MIN_WEIGHT {
        return None;
    }
    Some(to_composite((sum_x / total, sum_y / total), width))
}

/// B13 grid pixels -> 1 km composite grid pixels.
fn to_composite(position: (f64, f64), width: usize) -> Position {
    let scale = FULL_DISK_SIZE as f64 / width as f64;
    (position.0 * scale, position.1 * scale)
}
