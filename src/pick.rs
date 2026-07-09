//! Interactive storm picker for `--pick-storm`: an annotated preview of the
//! first scene's storm candidates, and a terminal prompt to choose one.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::compose::{Rgb8Image, FULL_DISK_SIZE};
use crate::track::Position;

const MARKER: [u8; 3] = [255, 60, 220];
/// Half-side of the marker box, in preview pixels.
const BOX: i64 = 40;

/// 3x5 digit glyphs, one row per byte, low three bits used.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b010, 0b010, 0b010],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

/// Show the annotated candidates and ask which to track. `lat_lons` pairs
/// with `candidates`; the preview is the full-resolution IR render.
pub fn choose(
    preview: &Rgb8Image,
    candidates: &[(Position, f64)],
    lat_lons: &[Option<(f64, f64)>],
    preview_path: &Path,
) -> Result<Position> {
    if !std::io::stdin().is_terminal() {
        bail!("--pick-storm needs an interactive terminal; use --follow-seed LAT,LON instead");
    }

    // Quarter-size preview with numbered markers.
    let mut image = shrink(preview, 4);
    let scale = f64::from(image.width) / FULL_DISK_SIZE as f64;
    for (index, (position, _)) in candidates.iter().enumerate() {
        draw_marker(
            &mut image,
            (position.0 * scale, position.1 * scale),
            index + 1,
        );
    }
    image::RgbImage::from_raw(image.width, image.height, image.data)
        .context("assembling preview buffer")?
        .save(preview_path)
        .with_context(|| format!("writing {}", preview_path.display()))?;

    // Best effort: pop the preview open in the platform viewer.
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = Command::new(opener).arg(preview_path).spawn();

    eprintln!("\nStorm candidates ({}):", preview_path.display());
    for (index, ((_, cold_weight), lat_lon)) in candidates.iter().zip(lat_lons).enumerate() {
        let place = match lat_lon {
            Some((lat, lon)) => format!(
                "{:5.1}°{} {:6.1}°{}",
                lat.abs(),
                if *lat >= 0.0 { 'N' } else { 'S' },
                lon.abs(),
                if *lon >= 0.0 { 'E' } else { 'W' },
            ),
            None => "off-disk?".to_string(),
        };
        eprintln!("  {}: {place}   (cold-cloud weight {cold_weight:.0})", index + 1);
    }

    let stdin = std::io::stdin();
    for _ in 0..3 {
        eprint!("Track which storm [1-{}]? ", candidates.len());
        std::io::stderr().flush().ok();
        let mut line = String::new();
        stdin.lock().read_line(&mut line).context("reading choice")?;
        if let Ok(choice) = line.trim().parse::<usize>()
            && (1..=candidates.len()).contains(&choice)
        {
            return Ok(candidates[choice - 1].0);
        }
        eprintln!("please enter a number between 1 and {}", candidates.len());
    }
    bail!("no valid choice made");
}

/// Integer box-downsample of an RGB image.
fn shrink(image: &Rgb8Image, factor: u32) -> Rgb8Image {
    let (w, h) = (image.width / factor, image.height / factor);
    let mut data = vec![0u8; (w * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let mut sum = [0u32; 3];
            for sy in 0..factor {
                let row = ((y * factor + sy) * image.width + x * factor) as usize * 3;
                for sx in 0..factor {
                    for (accumulated, &value) in
                        sum.iter_mut().zip(&image.data[row + sx as usize * 3..][..3])
                    {
                        *accumulated += u32::from(value);
                    }
                }
            }
            let out = ((y * w + x) * 3) as usize;
            for c in 0..3 {
                data[out + c] = (sum[c] / (factor * factor)) as u8;
            }
        }
    }
    Rgb8Image { width: w, height: h, data }
}

fn put(image: &mut Rgb8Image, x: i64, y: i64) {
    if x >= 0 && y >= 0 && (x as u32) < image.width && (y as u32) < image.height {
        let i = (y as u32 * image.width + x as u32) as usize * 3;
        image.data[i..i + 3].copy_from_slice(&MARKER);
    }
}

/// A box outline with crosshair ticks and the candidate number beside it.
fn draw_marker(image: &mut Rgb8Image, center: (f64, f64), number: usize) {
    let (cx, cy) = (center.0 as i64, center.1 as i64);
    for offset in -BOX..=BOX {
        for thick in 0..3 {
            put(image, cx + offset, cy - BOX + thick);
            put(image, cx + offset, cy + BOX - thick);
            put(image, cx - BOX + thick, cy + offset);
            put(image, cx + BOX - thick, cy + offset);
        }
    }
    for tick in 8..=BOX / 2 {
        for thick in 0..2 {
            put(image, cx + tick, cy + thick);
            put(image, cx - tick, cy + thick);
            put(image, cx + thick, cy + tick);
            put(image, cx + thick, cy - tick);
        }
    }

    // The number, scaled up, to the right of the box.
    const SCALE: i64 = 8;
    let mut pen_x = cx + BOX + 10;
    for digit in number.to_string().bytes() {
        let glyph = DIGITS[(digit - b'0') as usize];
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) != 0 {
                    for dy in 0..SCALE {
                        for dx in 0..SCALE {
                            put(
                                image,
                                pen_x + col as i64 * SCALE + dx,
                                cy - BOX + row as i64 * SCALE + dy,
                            );
                        }
                    }
                }
            }
        }
        pen_x += 4 * SCALE;
    }
}
