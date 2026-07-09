//! Assembly of per-band full-disk grids and product rendering.
//!
//! Layout of this file:
//!   - image types and segment assembly
//!   - `render_disk`: the shared per-pixel scaffold every RGB product uses
//!     (box-average bands, geometry lookup, then a product-specific shader)
//!   - the products: `true_color`, `natural_color`, `combined`,
//!     `ir_enhancement`, `cloud_top_height`
//!   - the grade: geometric corrections and display tone mapping
//!   - small sampling utilities
//!
//! All the numbers worth tweaking live in `src/tuning.rs`.
//!
//! Why no GPU path: the per-pixel work here is a table lookup, a handful of
//! multiply-adds, and one `powf` — the job is memory-bandwidth bound, not
//! compute bound. The ~1 GB of band grids would have to cross the PCIe/unified
//! memory bus just to do arithmetic a multicore CPU already overlaps with the
//! (much slower) network download and bzip2 decompression. Rayon across all
//! cores finishes the compositing in well under a second of the multi-minute
//! end-to-end run, so a GPU would add complexity with no wall-clock win.

use anyhow::{bail, Context, Result};
use rayon::prelude::*;

use crate::fetch::Band;
use crate::geo::{Angles, Geometry};
use crate::hsd::{Calibration, Projection, Segment};
use crate::tuning::*;

/// Full-disk width/height of the common 1 km grid all bands are placed on.
pub const FULL_DISK_SIZE: usize = 11_000;

// ---------------------------------------------------------------------------
// Image types and segment assembly
// ---------------------------------------------------------------------------

/// One band's raw counts assembled onto the full 1 km disk grid.
pub struct BandImage {
    pub band: Band,
    pub calibration: Calibration,
    pub projection: Projection,
    /// Row-major, `FULL_DISK_SIZE * FULL_DISK_SIZE` counts.
    pub counts: Vec<u16>,
}

/// An 8-bit RGB image, row-major.
pub struct Rgb8Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Which brightness-temperature palette to render with: `--clut-style` for
/// the standalone `--clut-bands` images, `--combined-style` for the
/// combined product's sandwich overlay. The `SANDWICH_*` thresholds are
/// tuned to `Convection`'s cold ramp, so that is the default everywhere;
/// other palettes simply recolor whatever the overlay paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ClutStyle {
    #[default]
    Convection,
    Grayscale,
    Rainbow,
    WaterVapor,
}

impl ClutStyle {
    fn table(self) -> &'static [(f32, [f32; 3])] {
        match self {
            ClutStyle::Convection => CLUT_CONVECTION,
            ClutStyle::Grayscale => CLUT_GRAYSCALE,
            ClutStyle::Rainbow => CLUT_RAINBOW,
            ClutStyle::WaterVapor => CLUT_WATER_VAPOR,
        }
    }
}

/// Stitch a band's 10 segments onto a `target_width`-wide full-disk grid.
/// Segments at exactly twice the target resolution (the 0.5 km red band on
/// the 1 km grid) are 2x2 box-averaged as they land.
pub fn assemble_counts(
    target_width: usize,
    mut segments: Vec<Segment>,
) -> Result<(Calibration, Projection, Vec<u16>)> {
    segments.sort_by_key(|s| s.segment_number);
    let calibration = segments[0].calibration;
    let projection = segments[0].projection;
    let mut counts = vec![calibration.outside_scan_count; target_width * target_width];

    for segment in segments {
        let (rows, first_line) = if segment.columns == 2 * target_width {
            if segment.first_line % 2 != 1 || segment.lines % 2 != 0 {
                bail!(
                    "band {}: segment {} not aligned for 2x reduction (first line {}, {} lines)",
                    segment.band_number,
                    segment.segment_number,
                    segment.first_line,
                    segment.lines,
                );
            }
            (
                halve_counts(&segment.counts, segment.columns, segment.lines, &calibration),
                (segment.first_line - 1) / 2 + 1,
            )
        } else if segment.columns == target_width {
            (segment.counts, segment.first_line)
        } else if segment.columns * 2 == target_width {
            // 2 km bands on the 1 km grid: duplicate pixels. The blockiness
            // is the sensor's real resolution, not an artifact.
            (
                double_counts(&segment.counts, segment.columns),
                (segment.first_line - 1) * 2 + 1,
            )
        } else {
            bail!(
                "band {}: segment width {} fits no multiple of a {}-pixel grid",
                segment.band_number,
                segment.columns,
                target_width,
            );
        };

        let start = (first_line - 1) * target_width;
        let end = start + rows.len();
        if first_line == 0 || end > counts.len() {
            bail!(
                "band {}: segment {} does not fit the full disk (lines {}..{})",
                segment.band_number,
                segment.segment_number,
                first_line,
                first_line + rows.len() / target_width,
            );
        }
        counts[start..end].copy_from_slice(&rows);
    }

    Ok((calibration, projection, counts))
}

/// Stitch a true-color band's segments onto the common 1 km disk grid.
pub fn assemble_band(band: Band, segments: Vec<Segment>) -> Result<BandImage> {
    let (calibration, projection, counts) = assemble_counts(FULL_DISK_SIZE, segments)?;
    Ok(BandImage { band, calibration, projection, counts })
}

/// 2x2 box-average of raw counts (calibration is affine, so averaging counts
/// equals averaging reflectances). Invalid sentinel counts are excluded; a
/// block with no valid pixel stays marked as outside-scan.
fn halve_counts(counts: &[u16], width: usize, lines: usize, cal: &Calibration) -> Vec<u16> {
    let out_width = width / 2;
    let mut out = vec![0u16; out_width * (lines / 2)];
    out.par_chunks_mut(out_width)
        .enumerate()
        .for_each(|(out_y, out_row)| {
            let top = &counts[2 * out_y * width..][..width];
            let bottom = &counts[(2 * out_y + 1) * width..][..width];
            for (out_x, out_px) in out_row.iter_mut().enumerate() {
                let mut sum = 0u32;
                let mut valid = 0u32;
                for count in [
                    top[2 * out_x],
                    top[2 * out_x + 1],
                    bottom[2 * out_x],
                    bottom[2 * out_x + 1],
                ] {
                    if cal.is_valid(count) {
                        sum += u32::from(count);
                        valid += 1;
                    }
                }
                *out_px = sum
                    .checked_div(valid)
                    .map_or(cal.outside_scan_count, |average| average as u16);
            }
        });
    out
}

/// 2x pixel duplication (each source pixel becomes a 2x2 block). Sentinel
/// counts duplicate along with everything else, keeping validity intact.
fn double_counts(counts: &[u16], width: usize) -> Vec<u16> {
    let out_width = width * 2;
    let lines = counts.len() / width;
    let mut out = vec![0u16; out_width * lines * 2];
    out.par_chunks_mut(out_width)
        .enumerate()
        .for_each(|(out_y, out_row)| {
            let source = &counts[(out_y / 2) * width..][..width];
            for (pair, &count) in out_row.chunks_exact_mut(2).zip(source) {
                pair.fill(count);
            }
        });
    out
}

// ---------------------------------------------------------------------------
// Shared per-pixel scaffold
// ---------------------------------------------------------------------------

/// Everything a product's shader gets to see for one output pixel.
struct PixelSample {
    out_x: usize,
    out_y: usize,
    /// Fractional 0-based coordinates of the pixel center on the 1 km grid.
    col: f64,
    line: f64,
    /// Box-averaged linear reflectances of the four true-color bands
    /// (red, green, blue, near-IR).
    rgbn: [f32; 4],
    angles: Angles,
}

/// The parallel walk every RGB disk product shares: box-average the four
/// bands over `factor` x `factor` blocks, look up the pixel's geometry, and
/// hand the result to a product-specific shader that returns display-space
/// RGB (0..1). Space, missing-data, and past-the-limb pixels stay black.
fn render_disk<F>(
    bands: [&BandImage; 4],
    geometry: &Geometry,
    factor: usize,
    shader: F,
) -> Result<Rgb8Image>
where
    F: Fn(&PixelSample) -> [f32; 3] + Sync,
{
    debug_assert!(factor >= 1 && FULL_DISK_SIZE.is_multiple_of(factor));
    let out_size = FULL_DISK_SIZE / factor;

    let lut = |band: &BandImage| {
        band.calibration
            .reflectance_lut()
            .with_context(|| format!("{} has no reflectance calibration", band.band.code()))
    };
    let luts = [lut(bands[0])?, lut(bands[1])?, lut(bands[2])?, lut(bands[3])?];

    let mut data = vec![0u8; out_size * out_size * 3];
    data.par_chunks_mut(out_size * 3)
        .enumerate()
        .for_each(|(out_y, out_row)| {
            for out_x in 0..out_size {
                let mut sum = [0f32; 4];
                let mut valid = 0u32;
                for sub_y in 0..factor {
                    let row_base = (out_y * factor + sub_y) * FULL_DISK_SIZE + out_x * factor;
                    for sub_x in 0..factor {
                        let i = row_base + sub_x;
                        let sample = [
                            luts[0][bands[0].counts[i] as usize],
                            luts[1][bands[1].counts[i] as usize],
                            luts[2][bands[2].counts[i] as usize],
                            luts[3][bands[3].counts[i] as usize],
                        ];
                        if sample.iter().all(|v| v.is_finite()) {
                            for (acc, v) in sum.iter_mut().zip(sample) {
                                *acc += v;
                            }
                            valid += 1;
                        }
                    }
                }
                if valid == 0 {
                    continue;
                }
                let col = (out_x as f64 + 0.5) * factor as f64 - 0.5;
                let line = (out_y as f64 + 0.5) * factor as f64 - 0.5;
                let Some(angles) = geometry.angles(col, line) else {
                    continue; // looks past the limb: leave black
                };
                let inv = 1.0 / valid as f32;
                let pixel = shader(&PixelSample {
                    out_x,
                    out_y,
                    col,
                    line,
                    rgbn: sum.map(|v| v * inv),
                    angles,
                });
                out_row[3 * out_x..3 * out_x + 3].copy_from_slice(&quantize(pixel));
            }
        });

    Ok(Rgb8Image {
        width: out_size as u32,
        height: out_size as u32,
        data,
    })
}

// ---------------------------------------------------------------------------
// Products
// ---------------------------------------------------------------------------

/// Composite the four bands into a gamma-encoded true-color image,
/// box-averaging `factor` x `factor` blocks of the 1 km grid per output
/// pixel. Space and missing pixels come out black.
pub fn true_color(
    red: &BandImage,
    green: &BandImage,
    blue: &BandImage,
    nir: &BandImage,
    geometry: &Geometry,
    factor: usize,
) -> Result<Rgb8Image> {
    render_disk([red, green, blue, nir], geometry, factor, |px| {
        tone_map(correct(hybrid_rgb(px.rgbn), &px.angles))
    })
}

/// Natural-color composite: 1.6 um / 0.86 um / 0.64 um as RGB. Snow, ice,
/// and ice-topped cloud absorb at 1.6 um and come out cyan; water cloud
/// stays white; vegetation, strongly reflective in the near-IR, goes vivid
/// green. The geometric corrections and grade are shared with true color
/// (the Rayleigh haze constants are tuned for the visible triple, but at
/// these longer wavelengths the veil is small anyway).
pub fn natural_color(
    swir: &BandImage,
    nir: &BandImage,
    red: &BandImage,
    geometry: &Geometry,
    factor: usize,
) -> Result<Rgb8Image> {
    render_disk([swir, nir, red, red], geometry, factor, |px| {
        let [s, n, r, _] = px.rgbn;
        tone_map(correct([s, n, r], &px.angles))
    })
}

/// Composite the true-color image with the B13 clean-IR band: a sandwich
/// product where cold cloud tops keep their CLUT colors on top of the
/// visible imagery, and the night side of the disk shows IR clouds in
/// grayscale instead of going black.
#[allow(clippy::too_many_arguments)]
pub fn combined(
    red: &BandImage,
    green: &BandImage,
    blue: &BandImage,
    nir: &BandImage,
    ir_calibration: &Calibration,
    ir_counts: &[u16],
    ir_width: usize,
    geometry: &Geometry,
    factor: usize,
    style: ClutStyle,
) -> Result<Rgb8Image> {
    // Precompute the brightness-temperature grid once so the per-pixel work
    // is a bilinear sample instead of a Planck inversion.
    let bt_lut = ir_calibration
        .brightness_temperature_lut()
        .context("combined product needs a thermal band")?;
    let bt_grid: Vec<f32> = ir_counts.iter().map(|&c| bt_lut[c as usize]).collect();
    // One IR pixel spans this many 1 km grid pixels (2 for the 2 km B13).
    let ir_scale = FULL_DISK_SIZE as f32 / ir_width as f32;

    render_disk([red, green, blue, nir], geometry, factor, |px| {
        let mut pixel = tone_map(correct(hybrid_rgb(px.rgbn), &px.angles));

        // IR coordinate of this output pixel's center.
        let center = (px.out_x as f32 + 0.5) * factor as f32;
        let ir_x = center / ir_scale - 0.5;
        let ir_y = ((px.out_y as f32 + 0.5) * factor as f32) / ir_scale - 0.5;
        let mut bt = sample_bilinear(&bt_grid, ir_width, ir_x, ir_y);
        // Parallax: a tall cloud top appears displaced away from the
        // sub-satellite point, so this pixel's cloud lives elsewhere in the
        // IR grid. Estimate its height from the first sample, then resample
        // at its apparent position.
        if bt.is_finite() {
            let height = ((PARALLAX_REF_TEMP - bt) / PARALLAX_LAPSE_K_PER_KM)
                .clamp(0.0, PARALLAX_MAX_HEIGHT_KM);
            if height > PARALLAX_MIN_HEIGHT_KM
                && let Some((app_col, app_line)) =
                    geometry.apparent_position(px.col, px.line, f64::from(height))
            {
                let shifted = sample_bilinear(
                    &bt_grid,
                    ir_width,
                    (app_col as f32 + 0.5) / ir_scale - 0.5,
                    (app_line as f32 + 0.5) / ir_scale - 0.5,
                );
                if shifted.is_finite() {
                    bt = shifted;
                }
            }
        }
        if !bt.is_finite() || bt < SPACE_BT_CUTOFF {
            return pixel;
        }

        // Night side: fade in grayscale IR clouds as the sun sets.
        let night = 1.0 - daylight(px.angles.cos_sun);
        if night > 0.0 {
            let gray = ((NIGHT_IR_WARM - bt) / (NIGHT_IR_WARM - NIGHT_IR_COLD)).clamp(0.0, 1.0);
            for channel in &mut pixel {
                *channel = channel.max(gray * night);
            }
        }

        // Sandwich overlay: CLUT colors for cold convective tops, faded out
        // at the limb where B13 reads falsely cold.
        if bt < SANDWICH_START {
            let half = FULL_DISK_SIZE as f32 / 2.0;
            let dx = (px.out_x as f32 + 0.5) * factor as f32 - half;
            let dy = (px.out_y as f32 + 0.5) * factor as f32 - half;
            let radius = (dx * dx + dy * dy).sqrt() / half;
            let limb =
                1.0 - smoothstep(((radius - LIMB_FADE_START) / LIMB_FADE_WIDTH).clamp(0.0, 1.0));
            let alpha =
                ((SANDWICH_START - bt) / SANDWICH_RAMP).clamp(0.0, 1.0) * SANDWICH_ALPHA * limb;
            let clut = clut_color(style.table(), bt).map(|c| f32::from(c) / 255.0);
            for (channel, overlay) in pixel.iter_mut().zip(clut) {
                *channel = *channel * (1.0 - alpha) + overlay * alpha;
            }
        }

        pixel
    })
}

/// Render an assembled thermal band through a brightness-temperature CLUT
/// at its native resolution.
pub fn ir_enhancement(
    calibration: &Calibration,
    counts: &[u16],
    width: usize,
    style: ClutStyle,
) -> Result<Rgb8Image> {
    let bt_lut = calibration
        .brightness_temperature_lut()
        .context("band has no thermal calibration")?;
    let rgb_lut: Vec<[u8; 3]> = bt_lut
        .iter()
        .map(|&bt| clut_color(style.table(), bt))
        .collect();
    Ok(render_count_lut(counts, width, &rgb_lut))
}

/// Cloud-top height render: B13 brightness temperature converted to height
/// with the fixed-lapse estimate and drawn through the hypsometric palette.
/// Warm (cloud-free) pixels render near-black. Note the estimate reads any
/// cold *surface* as cloud too — wintertime Antarctica shows up as false
/// high tops, a known limit of single-band height products.
pub fn cloud_top_height(
    calibration: &Calibration,
    counts: &[u16],
    width: usize,
) -> Result<Rgb8Image> {
    let bt_lut = calibration
        .brightness_temperature_lut()
        .context("cloud-top height needs a thermal band")?;
    let rgb_lut: Vec<[u8; 3]> = bt_lut
        .iter()
        .map(|&bt| {
            if !bt.is_finite() || bt < SPACE_BT_CUTOFF {
                return [0, 0, 0];
            }
            let height = ((PARALLAX_REF_TEMP - bt) / PARALLAX_LAPSE_K_PER_KM)
                .clamp(0.0, PARALLAX_MAX_HEIGHT_KM);
            if height < CLOUD_MIN_HEIGHT_KM {
                HEIGHT_CLEAR_RGB
            } else {
                table_color(HEIGHT_CLUT, height)
            }
        })
        .collect();
    Ok(render_count_lut(counts, width, &rgb_lut))
}

/// The parallel walk shared by every per-count-LUT product: each raw count
/// maps straight to a color.
fn render_count_lut(counts: &[u16], width: usize, rgb_lut: &[[u8; 3]]) -> Rgb8Image {
    let height = counts.len() / width;
    let mut data = vec![0u8; width * height * 3];
    data.par_chunks_mut(width * 3)
        .zip(counts.par_chunks(width))
        .for_each(|(out_row, count_row)| {
            for (pixel, &count) in out_row.chunks_exact_mut(3).zip(count_row) {
                pixel.copy_from_slice(&rgb_lut[count as usize]);
            }
        });

    Rgb8Image {
        width: width as u32,
        height: height as u32,
        data,
    }
}

// ---------------------------------------------------------------------------
// The grade: linear-light corrections and display tone mapping
// ---------------------------------------------------------------------------

/// The vegetation-corrected RGB triple from the four averaged bands.
fn hybrid_rgb([r, g, b, n]: [f32; 4]) -> [f32; 3] {
    [r, GREEN_FRACTION * g + NIR_FRACTION * n, b]
}

/// Daylight factor for a pixel: 1 in full sun, fading smoothly to 0 just
/// past the terminator.
fn daylight(cos_sun: f32) -> f32 {
    smoothstep(((cos_sun - DAYLIGHT_ZERO) / (DAYLIGHT_FULL - DAYLIGHT_ZERO)).clamp(0.0, 1.0))
}

/// Linear-light geometric corrections: sun-angle brightness normalization
/// first (turning values into partial reflectance factors), then Rayleigh
/// haze subtraction scaled by the view path, glint softening, and the
/// twilight fade with limb shading.
fn correct(rgb: [f32; 3], angles: &Angles) -> [f32; 3] {
    let cos_sun = angles.cos_sun.max(MIN_COS_SUN);
    let sun_norm = cos_sun.powf(SUN_CORRECTION);
    let view_airmass = (1.0 / angles.cos_view.max(MIN_COS_VIEW)).min(MAX_VIEW_AIRMASS);
    // The partial normalization leaves values scaled by cos^(1-S); the haze,
    // a real radiance riding on the same illumination, dims identically.
    let haze_illumination = cos_sun.powf(1.0 - SUN_CORRECTION);
    let limb_shade = LIMB_SHADE_FLOOR
        + (1.0 - LIMB_SHADE_FLOOR)
            * smoothstep((angles.cos_view / LIMB_SHADE_START).clamp(0.0, 1.0));
    let fade = daylight(angles.cos_sun) * limb_shade;

    // White veil subtracted around the specular point.
    let glint = GLINT_STRENGTH
        * smoothstep(((angles.cos_glint - GLINT_COS_EDGE) / (1.0 - GLINT_COS_EDGE)).clamp(0.0, 1.0));

    let mut corrected = [0f32; 3];
    for (out, (reflectance, haze_nadir)) in corrected.iter_mut().zip(rgb.into_iter().zip(HAZE_NADIR))
    {
        let haze_nominal = haze_nadir * view_airmass;
        let dehazed =
            (reflectance / sun_norm - haze_nominal * haze_illumination) / (1.0 - haze_nominal);
        *out = (dehazed - glint) * fade;
    }
    corrected
}

/// Gamma, S-curve contrast, and saturation boost; returns display-space
/// channels clamped to 0..1.
fn tone_map(rgb: [f32; 3]) -> [f32; 3] {
    let mut toned = [0f32; 3];
    for (channel, linear) in toned.iter_mut().zip(rgb) {
        let gamma = linear.clamp(0.0, 1.0).powf(1.0 / GAMMA);
        // Blend toward smoothstep: identity at 0 and 1, steeper mid-tones.
        let smooth = gamma * gamma * (3.0 - 2.0 * gamma);
        *channel = gamma + CONTRAST * (smooth - gamma);
    }
    // Rec. 709 luma; saturation scales each channel's distance from it.
    let luma = 0.2126 * toned[0] + 0.7152 * toned[1] + 0.0722 * toned[2];
    toned.map(|channel| (luma + SATURATION * (channel - luma)).clamp(0.0, 1.0))
}

/// Piecewise-linear sample of a (position, color) table, clamped at the
/// ends.
fn table_color(table: &[(f32, [f32; 3])], x: f32) -> [u8; 3] {
    let (floor, ceiling) = (table[0], table[table.len() - 1]);
    if x <= floor.0 {
        return floor.1.map(|c| c as u8);
    }
    if x >= ceiling.0 {
        return ceiling.1.map(|c| c as u8);
    }
    let above = table.partition_point(|(t, _)| *t <= x);
    let (t0, rgb0) = table[above - 1];
    let (t1, rgb1) = table[above];
    let blend = (x - t0) / (t1 - t0);
    let mut rgb = [0u8; 3];
    for ((out, low), high) in rgb.iter_mut().zip(rgb0).zip(rgb1) {
        *out = (low + blend * (high - low)).round() as u8;
    }
    rgb
}

/// Space, error, and implausibly cold pixels render as black.
fn clut_color(table: &[(f32, [f32; 3])], brightness_temp: f32) -> [u8; 3] {
    if !brightness_temp.is_finite() || brightness_temp < SPACE_BT_CUTOFF {
        return [0, 0, 0];
    }
    table_color(table, brightness_temp)
}

// ---------------------------------------------------------------------------
// Sampling utilities
// ---------------------------------------------------------------------------

fn quantize(rgb: [f32; 3]) -> [u8; 3] {
    rgb.map(|channel| (channel.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
}

fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Bilinear sample of a grid at fractional pixel-center coordinates,
/// clamped at the edges. NaN neighbors are dropped and the remaining
/// weights renormalized; NaN if no neighbor is valid.
fn sample_bilinear(grid: &[f32], width: usize, x: f32, y: f32) -> f32 {
    let height = grid.len() / width;
    let x = x.clamp(0.0, (width - 1) as f32);
    let y = y.clamp(0.0, (height - 1) as f32);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(width - 1), (y0 + 1).min(height - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);

    let mut sum = 0.0f32;
    let mut weight_total = 0.0f32;
    for (value, weight) in [
        (grid[y0 * width + x0], (1.0 - fx) * (1.0 - fy)),
        (grid[y0 * width + x1], fx * (1.0 - fy)),
        (grid[y1 * width + x0], (1.0 - fx) * fy),
        (grid[y1 * width + x1], fx * fy),
    ] {
        if value.is_finite() {
            sum += value * weight;
            weight_total += weight;
        }
    }
    if weight_total > 0.0 {
        sum / weight_total
    } else {
        f32::NAN
    }
}
