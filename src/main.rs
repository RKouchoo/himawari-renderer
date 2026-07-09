//! himawari-earth: download Himawari-9 full-disk scenes from the public
//! `noaa-himawari9` S3 bucket and render them — true color, natural color,
//! IR enhancements, cloud-top height, a combined day/night sandwich
//! product, and timelapse video.
//!
//! Every (band, segment) file is downloaded, decompressed, and parsed as a
//! parallel leaf task on the rayon thread pool; compositing is parallelized
//! per output row. Look tunables live in `tuning.rs`.

mod compose;
mod fetch;
mod geo;
mod hsd;
mod tuning;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, Timelike, Utc};
use clap::Parser;
use rayon::prelude::*;

use compose::{BandImage, FULL_DISK_SIZE};
use fetch::{Band, IrBand, SEGMENTS_PER_DISK};

#[derive(Parser, Debug)]
#[command(version, about = "True-color full-disk Earth image from Himawari-9 L1b data on AWS S3")]
struct Args {
    /// UTC observation time (YYYY-MM-DDTHH:MM, 10-minute slots).
    /// Defaults to the most recent scene available in the bucket.
    #[arg(long)]
    time: Option<String>,

    /// Output PNG path.
    #[arg(long, default_value = "earth.png")]
    out: PathBuf,

    /// Box-average factor applied to the 11000x11000 1 km grid; must divide
    /// 11000 (e.g. 1, 2, 4, 5, 8, 10). Defaults to 1 (full native
    /// resolution), or 4 in --timelapse mode (2750x2750 frames).
    #[arg(long)]
    downsample: Option<usize>,

    /// Worker thread count (defaults to all logical cores).
    #[arg(long)]
    threads: Option<usize>,

    /// Directory for caching downloaded .DAT.bz2 files across runs.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Thermal infrared bands (7-16, e.g. "13" or "B08,B13") to also render
    /// through a brightness-temperature color lookup table. Each band is
    /// written next to the main output as <out-stem>_Bnn.png at its native
    /// 2 km resolution (5500x5500).
    #[arg(long, value_delimiter = ',', value_name = "BAND", value_parser = parse_ir_band)]
    clut_bands: Vec<IrBand>,

    /// Palette for the --clut-bands images: "convection" highlights cold
    /// cloud tops over inverted grayscale, "grayscale" is the plain clean-IR
    /// look, "rainbow" colorizes the whole temperature range, and
    /// "water-vapor" suits bands 8-10. (The combined product's palette is
    /// --combined-style.)
    #[arg(long, value_enum, default_value_t = compose::ClutStyle::Convection)]
    clut_style: compose::ClutStyle,

    /// Also write <out-stem>_combined.png: the true-color composite with the
    /// B13 clean-IR band imposed on top. Cold cloud tops keep their CLUT
    /// colors (a "sandwich" product) and night-side clouds show as grayscale
    /// IR instead of going black.
    #[arg(long)]
    combined: bool,

    /// Palette for the combined product's sandwich overlay (same choices as
    /// --clut-style; the SANDWICH_* thresholds are tuned for convection).
    #[arg(long, value_enum, default_value_t = compose::ClutStyle::Convection)]
    combined_style: compose::ClutStyle,

    /// Also write <out-stem>_natural.png: a natural-color composite with the
    /// 1.6 um band as red (snow/ice cyan, water cloud white, vegetation
    /// vivid green). Fetches band B05 in addition to the core four.
    #[arg(long)]
    natural: bool,

    /// Also write <out-stem>_height.png: estimated cloud-top height from
    /// B13 through a hypsometric relief palette (green low cloud up to
    /// white at the tropopause). Fetches B13 if --combined has not already.
    #[arg(long)]
    cloud_height: bool,

    /// Render a timelapse video instead of a single scene: an inclusive UTC
    /// range of 10-minute slots, e.g. 2026-07-09T00:00..2026-07-09T06:00.
    /// Frames are the true-color disk, or the combined product when
    /// --combined is also given; the video lands at <out-stem>.mp4. Each
    /// scene is ~700 MB of downloads and its cache files are deleted as
    /// soon as its frame is rendered. Requires ffmpeg.
    #[arg(long, value_name = "START..END")]
    timelapse: Option<String>,

    /// Frames per second of the --timelapse video.
    #[arg(long, default_value_t = 12)]
    fps: u32,

    /// Keep running: poll the bucket and re-render every requested product
    /// to the same output paths whenever a new 10-minute scene appears.
    /// With --cache-dir, each scene's cached files are purged after use.
    #[arg(long)]
    watch: bool,
}

fn parse_ir_band(text: &str) -> Result<IrBand, String> {
    let digits = text.trim().trim_start_matches(['B', 'b']);
    let number: u8 = digits
        .parse()
        .map_err(|_| format!("invalid band {text:?}, expected a number like 13 or B13"))?;
    IrBand::new(number)
}

fn parse_time(text: &str) -> Result<DateTime<Utc>> {
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M")
        .with_context(|| format!("invalid --time {text:?}, expected YYYY-MM-DDTHH:MM"))?;
    if naive.minute() % 10 != 0 {
        bail!("--time minutes must be a multiple of 10 (full-disk scenes are 10-minutely)");
    }
    Ok(naive.and_utc())
}

fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(factor) = args.downsample {
        if factor == 0 || !FULL_DISK_SIZE.is_multiple_of(factor) {
            bail!("--downsample must divide {FULL_DISK_SIZE} (e.g. 1, 2, 4, 5, 8, 10)");
        }
    }
    if let Some(threads) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .context("configuring the thread pool")?;
    }
    if let Some(dir) = &args.cache_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating cache directory {}", dir.display()))?;
    }

    let agent = fetch::agent();
    if args.watch && (args.time.is_some() || args.timelapse.is_some()) {
        bail!("--watch cannot be combined with --time or --timelapse");
    }
    if let Some(range) = &args.timelapse {
        return run_timelapse(&agent, &args, range);
    }
    let downsample = args.downsample.unwrap_or(1);
    if args.watch {
        return run_watch(&agent, &args, downsample);
    }

    let (extra_bands, ir_needed) = required_bands(&args);
    let time = match &args.time {
        Some(text) => parse_time(text)?,
        None => fetch::find_latest_scene(&agent, &extra_bands, &ir_needed)
            .context("locating the latest scene")?,
    };
    run_scene(&agent, &args, time, downsample)
}

/// The bands beyond the core four that a run's flags require, for scene
/// completeness probing.
fn required_bands(args: &Args) -> (Vec<Band>, Vec<IrBand>) {
    let b13 = IrBand::new(13).expect("13 is a valid IR band");
    let mut ir_needed = args.clut_bands.clone();
    if (args.combined || args.cloud_height) && !ir_needed.contains(&b13) {
        ir_needed.push(b13);
    }
    let extra = if args.natural { vec![Band::B05] } else { Vec::new() };
    (extra, ir_needed)
}

/// Fetch one scene and render every requested product.
fn run_scene(
    agent: &ureq::Agent,
    args: &Args,
    time: DateTime<Utc>,
    downsample: usize,
) -> Result<()> {
    eprintln!("Scene: {} UTC", time.format("%Y-%m-%d %H:%M"));
    let started = Instant::now();

    let b13 = IrBand::new(13).expect("13 is a valid IR band");
    let need_b13 = args.combined || args.cloud_height;

    // Fetch and assemble all four bands; every (band, segment) leaf task runs
    // on the shared rayon pool, so downloads, bzip2 decompression, and HSD
    // parsing for all 40 files overlap.
    let bands: Vec<BandImage> = Band::ALL
        .into_par_iter()
        .map(|band| -> Result<BandImage> {
            let segments = (1..=SEGMENTS_PER_DISK)
                .into_par_iter()
                .map(|segment| {
                    fetch::fetch_segment(agent, time, band, segment, args.cache_dir.as_deref())
                })
                .collect::<Result<Vec<_>>>()?;
            compose::assemble_band(band, segments)
        })
        .collect::<Result<Vec<_>>>()?;
    eprintln!("All bands assembled after {:.1}s", started.elapsed().as_secs_f32());

    // B13 is shared by --combined and a possible --clut-bands 13; fetch and
    // assemble it once.
    let fetch_ir = |band: IrBand| -> Result<(hsd::Calibration, hsd::Projection, Vec<u16>)> {
        let segments = (1..=SEGMENTS_PER_DISK)
            .into_par_iter()
            .map(|segment| {
                fetch::fetch_segment(agent, time, band, segment, args.cache_dir.as_deref())
            })
            .collect::<Result<Vec<_>>>()?;
        compose::assemble_counts(IrBand::WIDTH, segments)
    };
    let b13_data = if need_b13 { Some(fetch_ir(b13)?) } else { None };

    // The 2 km B05 band for the natural-color product, upsampled onto the
    // 1 km grid during assembly.
    let b05_image = if args.natural {
        let segments = (1..=SEGMENTS_PER_DISK)
            .into_par_iter()
            .map(|segment| {
                fetch::fetch_segment(agent, time, Band::B05, segment, args.cache_dir.as_deref())
            })
            .collect::<Result<Vec<_>>>()?;
        Some(compose::assemble_band(Band::B05, segments)?)
    } else {
        None
    };

    let find = |band: Band| bands.iter().find(|b| b.band == band).unwrap();
    // B01's native grid is the composite grid, so its projection describes
    // the output pixels directly.
    let geometry = geo::Geometry::new(&find(Band::B01).projection, time);
    let image = compose::true_color(
        find(Band::B03),
        find(Band::B02),
        find(Band::B01),
        find(Band::B04),
        &geometry,
        downsample,
    )?;
    save_png(&args.out, image)?;

    if let (Some((calibration, _, counts)), true) = (&b13_data, args.combined) {
        let image = compose::combined(
            find(Band::B03),
            find(Band::B02),
            find(Band::B01),
            find(Band::B04),
            calibration,
            counts,
            IrBand::WIDTH,
            &geometry,
            downsample,
            args.combined_style,
        )?;
        save_png(&suffixed_output_path(&args.out, "combined"), image)?;
    }

    if let Some(b05) = &b05_image {
        let image = compose::natural_color(
            b05,
            find(Band::B04),
            find(Band::B03),
            &geometry,
            downsample,
        )?;
        save_png(&suffixed_output_path(&args.out, "natural"), image)?;
    }
    drop(b05_image);
    drop(bands);

    if let (Some((calibration, _, counts)), true) = (&b13_data, args.cloud_height) {
        let image = compose::cloud_top_height(calibration, counts, IrBand::WIDTH)?;
        save_png(&suffixed_output_path(&args.out, "height"), image)?;
    }

    // Render any requested thermal bands through the CLUT, again with every
    // (band, segment) fetch as a parallel leaf task.
    args.clut_bands
        .par_iter()
        .try_for_each(|&ir_band| -> Result<()> {
            let fetched;
            let (calibration, counts) = match &b13_data {
                Some((cal, _, counts)) if ir_band == b13 => (cal, counts.as_slice()),
                _ => {
                    fetched = fetch_ir(ir_band)?;
                    (&fetched.0, fetched.2.as_slice())
                }
            };
            let image =
                compose::ir_enhancement(calibration, counts, IrBand::WIDTH, args.clut_style)?;
            save_png(&ir_output_path(&args.out, ir_band), image)
        })?;

    eprintln!("Done in {:.1}s total", started.elapsed().as_secs_f32());
    Ok(())
}

/// Timelapse mode: one frame per 10-minute scene across the given range,
/// encoded to <out-stem>.mp4. Scenes are heavy (~700 MB each), so every
/// scene's cached files are deleted as soon as its frame is rendered, and
/// the frame PNGs are deleted after encoding.
fn run_timelapse(agent: &ureq::Agent, args: &Args, range: &str) -> Result<()> {
    let (start, end) = range
        .split_once("..")
        .context("--timelapse expects START..END, e.g. 2026-07-09T00:00..2026-07-09T06:00")
        .and_then(|(a, b)| Ok((parse_time(a)?, parse_time(b)?)))?;
    if end < start {
        bail!("--timelapse end is before start");
    }
    if !(1..=120).contains(&args.fps) {
        bail!("--fps must be between 1 and 120");
    }
    let downsample = args.downsample.unwrap_or(tuning::TIMELAPSE_DOWNSAMPLE);
    let frame_size = FULL_DISK_SIZE / downsample;
    if !frame_size.is_multiple_of(2) || frame_size > 5_500 {
        bail!(
            "--downsample {downsample} gives {frame_size}x{frame_size} frames; video needs even \
             dimensions of at most 5500 (try 4, 5, or 10)"
        );
    }
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        bail!("--timelapse needs ffmpeg on the PATH (e.g. `brew install ffmpeg`)");
    }

    let total_slots = ((end - start).num_minutes() / 10 + 1) as usize;
    eprintln!(
        "Timelapse: {} slots from {} to {} UTC ({} frames/s, ~{} GB of downloads)",
        total_slots,
        start.format("%Y-%m-%d %H:%M"),
        end.format("%Y-%m-%d %H:%M"),
        args.fps,
        total_slots * 7 / 10,
    );

    let frames_dir = args.out.with_file_name(format!(
        "{}_frames",
        args.out.file_stem().and_then(|s| s.to_str()).unwrap_or("earth"),
    ));
    std::fs::create_dir_all(&frames_dir)
        .with_context(|| format!("creating {}", frames_dir.display()))?;
    let cache_dir = args
        .cache_dir
        .clone()
        .unwrap_or_else(|| frames_dir.join("cache"));
    std::fs::create_dir_all(&cache_dir)?;

    let slots: Vec<DateTime<Utc>> = (0..total_slots as i64)
        .map(|i| start + chrono::Duration::minutes(10 * i))
        .collect();

    let started = Instant::now();
    // Pipeline: TIMELAPSE_PREFETCH producer threads download and assemble
    // scenes ahead of the consumer below, which renders frames in slot
    // order (a small reorder buffer absorbs producers finishing out of
    // order). Downloads dominate wall time, so overlapping them is close
    // to a direct throughput multiplier.
    let next_slot = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::sync_channel::<(usize, Result<Scene>)>(1);
    let rendered = thread::scope(|scope| -> Result<usize> {
        for _ in 0..tuning::TIMELAPSE_PREFETCH {
            let sender = sender.clone();
            let (next_slot, slots, cache_dir) = (&next_slot, &slots, &cache_dir);
            scope.spawn(move || loop {
                let index = next_slot.fetch_add(1, Ordering::SeqCst);
                let Some(&slot) = slots.get(index) else { break };
                let scene = fetch_scene(agent, slot, args.combined, cache_dir);
                purge_scene_cache(cache_dir, slot);
                if sender.send((index, scene)).is_err() {
                    break; // the consumer bailed
                }
            });
        }
        drop(sender);

        let mut out_of_order: HashMap<usize, Result<Scene>> = HashMap::new();
        let mut rendered = 0usize;
        for expected in 0..slots.len() {
            let scene = loop {
                if let Some(scene) = out_of_order.remove(&expected) {
                    break scene;
                }
                let (index, scene) = receiver
                    .recv()
                    .context("prefetch threads exited unexpectedly")?;
                out_of_order.insert(index, scene);
            };
            let frame_path = frames_dir.join(format!("frame_{:05}.png", rendered + 1));
            let outcome =
                scene.and_then(|scene| render_scene_frame(&scene, args, downsample, &frame_path));
            match outcome {
                Ok(()) => {
                    rendered += 1;
                    eprintln!(
                        "[{}/{}] {} UTC done ({:.0}s elapsed)",
                        expected + 1,
                        slots.len(),
                        slots[expected].format("%H:%M"),
                        started.elapsed().as_secs_f32(),
                    );
                }
                Err(error) => eprintln!(
                    "[{}/{}] {} UTC skipped: {error:#}",
                    expected + 1,
                    slots.len(),
                    slots[expected].format("%H:%M"),
                ),
            }
        }
        Ok(rendered)
    })?;
    if rendered == 0 {
        bail!("no scene in the range could be rendered");
    }

    let video = args.out.with_extension("mp4");
    let status = Command::new("ffmpeg")
        .args(["-y", "-framerate", &args.fps.to_string(), "-i"])
        .arg(frames_dir.join("frame_%05d.png"))
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .args(["-crf", &tuning::VIDEO_CRF.to_string()])
        .args(["-movflags", "+faststart"])
        .arg(&video)
        .status()
        .context("running ffmpeg")?;
    if !status.success() {
        bail!(
            "ffmpeg failed; the frames are kept in {} for manual encoding",
            frames_dir.display(),
        );
    }
    std::fs::remove_dir_all(&frames_dir)
        .with_context(|| format!("cleaning up {}", frames_dir.display()))?;

    eprintln!(
        "Wrote {} ({} frames at {} fps) in {:.0}s total",
        video.display(),
        rendered,
        args.fps,
        started.elapsed().as_secs_f32(),
    );
    Ok(())
}

/// Watch mode: poll the bucket and render every new scene as it appears,
/// overwriting the same output paths. Runs until interrupted.
fn run_watch(agent: &ureq::Agent, args: &Args, downsample: usize) -> Result<()> {
    let (extra_bands, ir_needed) = required_bands(args);
    eprintln!(
        "Watching for new scenes (polling every {}s, ctrl-c to stop)",
        tuning::WATCH_POLL_SECS,
    );
    let mut last_rendered: Option<DateTime<Utc>> = None;
    loop {
        match fetch::find_latest_scene(agent, &extra_bands, &ir_needed) {
            Ok(slot) if last_rendered != Some(slot) => {
                match run_scene(agent, args, slot, downsample) {
                    Ok(()) => {
                        last_rendered = Some(slot);
                        if let Some(cache) = &args.cache_dir {
                            purge_scene_cache(cache, slot);
                        }
                    }
                    Err(error) => {
                        eprintln!("scene {} failed: {error:#}", slot.format("%H:%M"));
                    }
                }
            }
            Ok(_) => {} // still the scene we already rendered
            Err(error) => eprintln!("scene probe failed: {error:#}"),
        }
        thread::sleep(StdDuration::from_secs(tuning::WATCH_POLL_SECS));
    }
}

/// One fetched-and-assembled scene, ready to render into a frame.
struct Scene {
    time: DateTime<Utc>,
    bands: Vec<BandImage>,
    b13: Option<(hsd::Calibration, hsd::Projection, Vec<u16>)>,
}

/// Fetch and assemble everything a timelapse frame needs.
fn fetch_scene(
    agent: &ureq::Agent,
    time: DateTime<Utc>,
    with_b13: bool,
    cache_dir: &Path,
) -> Result<Scene> {
    let bands: Vec<BandImage> = Band::ALL
        .into_par_iter()
        .map(|band| -> Result<BandImage> {
            let segments = (1..=SEGMENTS_PER_DISK)
                .into_par_iter()
                .map(|segment| fetch::fetch_segment(agent, time, band, segment, Some(cache_dir)))
                .collect::<Result<Vec<_>>>()?;
            compose::assemble_band(band, segments)
        })
        .collect::<Result<Vec<_>>>()?;
    let b13 = if with_b13 {
        let segments = (1..=SEGMENTS_PER_DISK)
            .into_par_iter()
            .map(|segment| {
                fetch::fetch_segment(
                    agent,
                    time,
                    IrBand::new(13).expect("13 is a valid IR band"),
                    segment,
                    Some(cache_dir),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        Some(compose::assemble_counts(IrBand::WIDTH, segments)?)
    } else {
        None
    };
    Ok(Scene { time, bands, b13 })
}

/// Render one timelapse frame from an assembled scene: the true-color disk,
/// or the combined product when --combined is set.
fn render_scene_frame(
    scene: &Scene,
    args: &Args,
    downsample: usize,
    frame_path: &Path,
) -> Result<()> {
    let find = |band: Band| scene.bands.iter().find(|b| b.band == band).unwrap();
    let geometry = geo::Geometry::new(&find(Band::B01).projection, scene.time);
    let image = if let Some((calibration, _, counts)) = &scene.b13 {
        compose::combined(
            find(Band::B03),
            find(Band::B02),
            find(Band::B01),
            find(Band::B04),
            calibration,
            counts,
            IrBand::WIDTH,
            &geometry,
            downsample,
            args.combined_style,
        )?
    } else {
        compose::true_color(
            find(Band::B03),
            find(Band::B02),
            find(Band::B01),
            find(Band::B04),
            &geometry,
            downsample,
        )?
    };
    save_png(frame_path, image)
}

/// Delete every cached .DAT.bz2 belonging to one observation slot.
fn purge_scene_cache(cache_dir: &Path, time: DateTime<Utc>) {
    let stamp = time.format("_%Y%m%d_%H%M_").to_string();
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("HS_H09") && name.contains(&stamp) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// "earth.png" + "natural" -> "earth_natural.png", next to the main output.
fn suffixed_output_path(out: &Path, suffix: &str) -> PathBuf {
    let stem = out
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("earth");
    out.with_file_name(format!("{stem}_{suffix}.png"))
}

fn ir_output_path(out: &Path, band: IrBand) -> PathBuf {
    suffixed_output_path(out, &band.to_string())
}

fn save_png(path: &Path, image: compose::Rgb8Image) -> Result<()> {
    let (width, height) = (image.width, image.height);
    image::RgbImage::from_raw(width, height, image.data)
        .context("assembling output image buffer")?
        .save(path)
        .with_context(|| format!("writing {}", path.display()))?;
    eprintln!("Wrote {} ({}x{})", path.display(), width, height);
    Ok(())
}
