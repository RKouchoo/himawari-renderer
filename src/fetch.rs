//! Anonymous download of Himawari-9 AHI L1b full-disk segments from the
//! public `noaa-himawari9` S3 bucket, plus bzip2 decompression and HSD
//! parsing of each segment.

use std::io::Read;
use std::path::Path;
use std::time::Duration as StdDuration;

use anyhow::{bail, Context, Result};
use bzip2::read::MultiBzDecoder;
use chrono::{DateTime, Duration, DurationRound, Utc};

use crate::hsd;
use crate::tuning::{
    CONNECT_TIMEOUT_SECS, DOWNLOAD_CONNECTIONS_PER_FILE, DOWNLOAD_RETRIES,
    DOWNLOAD_SPLIT_MIN_BYTES, READ_TIMEOUT_SECS, SCENE_LOOKBACK_SLOTS,
};

const BUCKET_URL: &str = "https://noaa-himawari9.s3.amazonaws.com";
pub const SEGMENTS_PER_DISK: u8 = 10;

/// Any AHI band that can be fetched from the bucket: enough identity to
/// build the HSD file name and validate the parsed geometry against it.
pub trait AhiBand: Copy + Send + Sync {
    fn number(self) -> u16;
    /// Band code used in file names, e.g. "B13".
    fn code(self) -> String;
    /// Resolution code used in file names (R05 = 0.5 km, R10 = 1 km,
    /// R20 = 2 km).
    fn resolution_code(self) -> &'static str;
    /// Full-disk width in pixels at the band's native resolution.
    fn native_width(self) -> usize;
}

/// The visible / near-IR bands the composites use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Band {
    /// 0.47 um, blue, 1 km
    B01,
    /// 0.51 um, green, 1 km
    B02,
    /// 0.64 um, red, 0.5 km
    B03,
    /// 0.86 um, near-IR (vegetation correction), 1 km
    B04,
    /// 1.6 um, shortwave IR (snow/ice discrimination), 2 km
    B05,
}

impl Band {
    /// The four bands every run fetches for the true-color composite.
    /// B05 is fetched on demand for the natural-color product.
    pub const ALL: [Band; 4] = [Band::B01, Band::B02, Band::B03, Band::B04];

    pub fn code(self) -> &'static str {
        match self {
            Band::B01 => "B01",
            Band::B02 => "B02",
            Band::B03 => "B03",
            Band::B04 => "B04",
            Band::B05 => "B05",
        }
    }

    /// Full-disk width in pixels at the band's native resolution.
    pub fn native_width(self) -> usize {
        match self {
            Band::B03 => 22_000,
            Band::B05 => 5_500,
            _ => 11_000,
        }
    }
}

impl AhiBand for Band {
    fn number(self) -> u16 {
        match self {
            Band::B01 => 1,
            Band::B02 => 2,
            Band::B03 => 3,
            Band::B04 => 4,
            Band::B05 => 5,
        }
    }

    fn code(self) -> String {
        Band::code(self).to_owned()
    }

    fn resolution_code(self) -> &'static str {
        match self {
            Band::B03 => "R05",
            Band::B05 => "R20",
            _ => "R10",
        }
    }

    fn native_width(self) -> usize {
        Band::native_width(self)
    }
}

/// A thermal infrared AHI band (B07-B16), all at 2 km resolution.
/// The inner number is validated at construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IrBand(u8);

impl IrBand {
    /// Full-disk width in pixels at 2 km resolution.
    pub const WIDTH: usize = 5_500;

    pub fn new(number: u8) -> Result<Self, String> {
        if (7..=16).contains(&number) {
            Ok(IrBand(number))
        } else {
            Err(format!("infrared band number must be 7-16, got {number}"))
        }
    }
}

impl std::fmt::Display for IrBand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "B{:02}", self.0)
    }
}

impl AhiBand for IrBand {
    fn number(self) -> u16 {
        u16::from(self.0)
    }

    fn code(self) -> String {
        self.to_string()
    }

    fn resolution_code(self) -> &'static str {
        "R20"
    }

    fn native_width(self) -> usize {
        Self::WIDTH
    }
}

pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(StdDuration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout_read(StdDuration::from_secs(READ_TIMEOUT_SECS))
        .user_agent(concat!("himawari-earth/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn file_name<B: AhiBand>(time: DateTime<Utc>, band: B, segment: u8) -> String {
    format!(
        "HS_H09_{}_{}_{}_FLDK_{}_S{:02}{:02}.DAT.bz2",
        time.format("%Y%m%d"),
        time.format("%H%M"),
        band.code(),
        band.resolution_code(),
        segment,
        SEGMENTS_PER_DISK,
    )
}

fn object_url<B: AhiBand>(time: DateTime<Utc>, band: B, segment: u8) -> String {
    format!(
        "{}/AHI-L1b-FLDK/{}/{}",
        BUCKET_URL,
        time.format("%Y/%m/%d/%H%M"),
        file_name(time, band, segment),
    )
}

/// True if every file this run needs (the core true-color bands plus any
/// extra visible and IR bands, 10 segments each) is already published for
/// the given observation slot. Scenes upload file by file over minutes, so
/// probing a single object is not enough; one ListObjectsV2 request returns
/// the slot's whole listing (160 keys max, under the 1000-key page limit)
/// and we substring-match the names we need.
fn scene_is_complete(
    agent: &ureq::Agent,
    slot: DateTime<Utc>,
    extra_bands: &[Band],
    ir_bands: &[IrBand],
) -> Result<bool> {
    let listing = agent
        .get(&format!("{BUCKET_URL}/"))
        .query("list-type", "2")
        .query("prefix", &format!("AHI-L1b-FLDK/{}/", slot.format("%Y/%m/%d/%H%M")))
        .call()
        .context("listing S3 objects for a scene slot")?
        .into_string()
        .context("reading S3 listing response")?;
    let visible_complete = Band::ALL.iter().chain(extra_bands).all(|&band| {
        (1..=SEGMENTS_PER_DISK).all(|segment| listing.contains(&file_name(slot, band, segment)))
    });
    let ir_complete = ir_bands.iter().all(|&band| {
        (1..=SEGMENTS_PER_DISK).all(|segment| listing.contains(&file_name(slot, band, segment)))
    });
    Ok(visible_complete && ir_complete)
}

/// Find the most recent complete full-disk scene in the bucket, walking back
/// in 10-minute observation slots from now. Slots can be missing or partial
/// due to publishing latency, and missing entirely in the twice-daily
/// housekeeping windows (around 02:40 and 14:40 UTC).
pub fn find_latest_scene(
    agent: &ureq::Agent,
    extra_bands: &[Band],
    ir_bands: &[IrBand],
) -> Result<DateTime<Utc>> {
    let mut slot = Utc::now()
        .duration_trunc(Duration::minutes(10))
        .context("truncating current time to a 10-minute slot")?;
    for _ in 0..SCENE_LOOKBACK_SLOTS {
        if scene_is_complete(agent, slot, extra_bands, ir_bands)? {
            return Ok(slot);
        }
        slot -= Duration::minutes(10);
    }
    bail!(
        "no complete full-disk scene found in the last {} hours of 10-minute slots",
        SCENE_LOOKBACK_SLOTS / 6,
    );
}

/// Download one object, splitting large files into byte ranges fetched on
/// DOWNLOAD_CONNECTIONS_PER_FILE concurrent connections. Files are already
/// parallel with each other; the split lifts the per-connection throughput
/// cap on each individual object as well.
fn download(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let length = object_length(agent, url)?;
    if DOWNLOAD_CONNECTIONS_PER_FILE <= 1 || length < DOWNLOAD_SPLIT_MIN_BYTES {
        return download_whole(agent, url, length as usize);
    }

    let mut buffer = vec![0u8; length as usize];
    let range_len = length.div_ceil(DOWNLOAD_CONNECTIONS_PER_FILE) as usize;
    std::thread::scope(|scope| -> Result<()> {
        let workers: Vec<_> = buffer
            .chunks_mut(range_len)
            .enumerate()
            .map(|(index, slice)| {
                let offset = (index * range_len) as u64;
                scope.spawn(move || download_range(agent, url, offset, slice))
            })
            .collect();
        for worker in workers {
            worker.join().expect("download worker panicked")?;
        }
        Ok(())
    })?;
    Ok(buffer)
}

/// Object size from a HEAD request. Fails fast on missing objects: a slot
/// that is not in the bucket will not appear on retry.
fn object_length(agent: &ureq::Agent, url: &str) -> Result<u64> {
    let mut last_err = None;
    for attempt in 0..DOWNLOAD_RETRIES {
        if attempt > 0 {
            std::thread::sleep(StdDuration::from_secs(2 << attempt));
        }
        match agent.head(url).call() {
            Ok(response) => {
                return response
                    .header("Content-Length")
                    .and_then(|v| v.parse::<u64>().ok())
                    .with_context(|| format!("{url}: response carries no Content-Length"));
            }
            Err(e @ ureq::Error::Status(403 | 404, _)) => {
                return Err(anyhow::Error::from(e)).with_context(|| {
                    format!(
                        "{url} does not exist; this observation slot may be a \
                         housekeeping window (~02:40 / ~14:40 UTC) or not yet published"
                    )
                });
            }
            Err(e) => last_err = Some(anyhow::Error::from(e)),
        }
    }
    Err(last_err.unwrap()).with_context(|| format!("probing {url} failed after retries"))
}

fn download_whole(agent: &ureq::Agent, url: &str, capacity: usize) -> Result<Vec<u8>> {
    let mut last_err = None;
    for attempt in 0..DOWNLOAD_RETRIES {
        if attempt > 0 {
            std::thread::sleep(StdDuration::from_secs(2 << attempt));
        }
        match agent.get(url).call() {
            Ok(response) => {
                let mut body = Vec::with_capacity(capacity);
                match response.into_reader().read_to_end(&mut body) {
                    Ok(_) => return Ok(body),
                    Err(e) => last_err = Some(anyhow::Error::from(e)),
                }
            }
            Err(e) => last_err = Some(anyhow::Error::from(e)),
        }
    }
    Err(last_err.unwrap()).with_context(|| format!("downloading {url} failed after retries"))
}

/// Fetch one byte range into its slice of the shared buffer.
fn download_range(agent: &ureq::Agent, url: &str, offset: u64, buf: &mut [u8]) -> Result<()> {
    let range = format!("bytes={}-{}", offset, offset + buf.len() as u64 - 1);
    let mut last_err = None;
    for attempt in 0..DOWNLOAD_RETRIES {
        if attempt > 0 {
            std::thread::sleep(StdDuration::from_secs(2 << attempt));
        }
        match agent.get(url).set("Range", &range).call() {
            Ok(response) if response.status() == 206 => {
                match read_exactly(response.into_reader(), buf) {
                    Ok(()) => return Ok(()),
                    Err(e) => last_err = Some(e),
                }
            }
            Ok(response) => {
                last_err = Some(anyhow::anyhow!(
                    "expected 206 Partial Content, got {}",
                    response.status(),
                ));
            }
            Err(e) => last_err = Some(anyhow::Error::from(e)),
        }
    }
    Err(last_err.unwrap()).with_context(|| format!("downloading {url} ({range}) failed after retries"))
}

fn read_exactly(mut reader: impl Read, buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => bail!("connection closed after {filled} of {} bytes", buf.len()),
            n => filled += n,
        }
    }
    Ok(())
}

/// Download (or read from cache), decompress, and parse one segment.
pub fn fetch_segment<B: AhiBand>(
    agent: &ureq::Agent,
    time: DateTime<Utc>,
    band: B,
    segment: u8,
    cache_dir: Option<&Path>,
) -> Result<hsd::Segment> {
    let name = file_name(time, band, segment);
    let cache_path = cache_dir.map(|dir| dir.join(&name));

    let compressed = match &cache_path {
        Some(path) if path.is_file() => {
            std::fs::read(path).with_context(|| format!("reading cached {}", path.display()))?
        }
        _ => {
            let bytes = download(agent, &object_url(time, band, segment))?;
            if let Some(path) = &cache_path {
                std::fs::write(path, &bytes)
                    .with_context(|| format!("writing cache file {}", path.display()))?;
            }
            bytes
        }
    };

    let mut raw = Vec::with_capacity(compressed.len() * 4);
    MultiBzDecoder::new(compressed.as_slice())
        .read_to_end(&mut raw)
        .with_context(|| format!("decompressing {name}"))?;

    let parsed = hsd::parse(&raw).with_context(|| format!("parsing {name}"))?;
    if parsed.band_number != band.number()
        || parsed.segment_number != segment
        || parsed.total_segments != SEGMENTS_PER_DISK
        || parsed.columns != band.native_width()
    {
        bail!(
            "{name}: unexpected contents (band {}, segment {}/{}, {} columns)",
            parsed.band_number,
            parsed.segment_number,
            parsed.total_segments,
            parsed.columns,
        );
    }
    eprintln!("  {} segment {:02}/{} ready", band.code(), segment, SEGMENTS_PER_DISK);
    Ok(parsed)
}
