//! Parser for the JMA Himawari Standard Data (HSD) format, as distributed in
//! the `noaa-himawari9` S3 bucket (one bzip2-compressed `.DAT` file per band
//! and full-disk segment).
//!
//! Only the header blocks needed for image reconstruction are decoded:
//!   block 2 — data information (pixel geometry)
//!   block 5 — calibration (count -> radiance -> reflectance)
//!   block 7 — segment information (where this segment sits in the full disk)

use thiserror::Error;

#[derive(Debug, Error)]
pub enum HsdError {
    #[error("file truncated: need {need} bytes at offset {offset}, have {have}")]
    Truncated {
        offset: usize,
        need: usize,
        have: usize,
    },
    #[error("not an HSD file: first header block number is {0}, expected 1")]
    NotHsd(u8),
    #[error("big-endian HSD files are not supported")]
    BigEndian,
    #[error("unsupported bits per pixel: {0} (expected 16)")]
    BitsPerPixel(u16),
    #[error("missing required header block {0}")]
    MissingBlock(u8),
    #[error("data block too small: expected {expected} pixels, have {have}")]
    DataSize { expected: usize, have: usize },
}

/// Bounds-checked little-endian reads over the raw file.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn bytes<const N: usize>(&self, offset: usize) -> Result<[u8; N], HsdError> {
        self.buf
            .get(offset..offset + N)
            .map(|s| <[u8; N]>::try_from(s).unwrap())
            .ok_or(HsdError::Truncated {
                offset,
                need: N,
                have: self.buf.len(),
            })
    }

    fn u8(&self, offset: usize) -> Result<u8, HsdError> {
        Ok(u8::from_le_bytes(self.bytes(offset)?))
    }

    fn u16(&self, offset: usize) -> Result<u16, HsdError> {
        Ok(u16::from_le_bytes(self.bytes(offset)?))
    }

    fn u32(&self, offset: usize) -> Result<u32, HsdError> {
        Ok(u32::from_le_bytes(self.bytes(offset)?))
    }

    fn f32(&self, offset: usize) -> Result<f32, HsdError> {
        Ok(f32::from_le_bytes(self.bytes(offset)?))
    }

    fn f64(&self, offset: usize) -> Result<f64, HsdError> {
        Ok(f64::from_le_bytes(self.bytes(offset)?))
    }
}

/// Normalized geostationary projection of the band's native grid (header
/// block 3): everything needed to invert pixel coordinates to latitude and
/// longitude and to reconstruct the viewing geometry.
#[derive(Debug, Clone, Copy)]
pub struct Projection {
    /// Sub-satellite longitude, degrees east.
    pub sub_lon_deg: f64,
    /// Column/line scaling factors and offsets (CFAC/LFAC/COFF/LOFF).
    pub cfac: u32,
    pub lfac: u32,
    pub coff: f64,
    pub loff: f64,
    /// Distance from Earth center to the satellite, km.
    pub distance_km: f64,
    /// Earth equatorial and polar radii, km.
    pub equatorial_radius_km: f64,
    pub polar_radius_km: f64,
}

/// Radiometric calibration (header block 5). Counts convert to radiance
/// with the affine `gain`/`offset`; the band-specific `kind` then carries
/// the coefficients for reflectance (bands 1-6) or brightness temperature
/// (bands 7-16).
#[derive(Debug, Clone, Copy)]
pub struct Calibration {
    gain: f64,
    offset: f64,
    pub error_count: u16,
    pub outside_scan_count: u16,
    kind: CalibrationKind,
}

#[derive(Debug, Clone, Copy)]
enum CalibrationKind {
    /// Visible / near-IR: `reflectance = albedo_coeff * radiance`.
    Reflective { albedo_coeff: f64 },
    /// Thermal IR: inverse Planck at the band's central wavelength gives an
    /// effective temperature, corrected to brightness temperature with the
    /// quadratic `rad_to_bt` coefficients. The physical constants are read
    /// from the file rather than hard-coded, matching JMA's processing.
    Emissive {
        wavelength_um: f64,
        rad_to_bt: [f64; 3],
        speed_of_light: f64,
        planck: f64,
        boltzmann: f64,
    },
}

impl Calibration {
    pub fn is_valid(&self, count: u16) -> bool {
        count != self.error_count && count != self.outside_scan_count
    }

    /// Lookup table mapping every possible 16-bit count to reflectance;
    /// invalid sentinel counts map to NaN. Indexing a table is faster than
    /// recomputing the transform per pixel and keeps the inner compositing
    /// loop branch-free. None for thermal bands.
    pub fn reflectance_lut(&self) -> Option<Vec<f32>> {
        let CalibrationKind::Reflective { albedo_coeff } = self.kind else {
            return None;
        };
        Some(
            (0..=u16::MAX)
                .map(|count| {
                    if self.is_valid(count) {
                        (albedo_coeff * (self.gain * f64::from(count) + self.offset)) as f32
                    } else {
                        f32::NAN
                    }
                })
                .collect(),
        )
    }

    /// Lookup table mapping every possible 16-bit count to brightness
    /// temperature in kelvin; invalid counts and non-positive radiances map
    /// to NaN. None for reflective bands.
    pub fn brightness_temperature_lut(&self) -> Option<Vec<f32>> {
        let CalibrationKind::Emissive {
            wavelength_um,
            rad_to_bt: [c0, c1, c2],
            speed_of_light: c,
            planck: h,
            boltzmann: k,
        } = self.kind
        else {
            return None;
        };
        let wavelength = wavelength_um * 1e-6;
        Some(
            (0..=u16::MAX)
                .map(|count| {
                    if !self.is_valid(count) {
                        return f32::NAN;
                    }
                    // Radiance in W m-2 sr-1 um-1; the 1e6 factor converts
                    // to per-meter for the Planck relation.
                    let radiance = self.gain * f64::from(count) + self.offset;
                    if radiance <= 0.0 {
                        return f32::NAN;
                    }
                    let a = h * c / (k * wavelength);
                    let b = 2.0 * h * c * c / (wavelength.powi(5) * radiance * 1e6) + 1.0;
                    let effective = a / b.ln();
                    (c0 + c1 * effective + c2 * effective * effective) as f32
                })
                .collect(),
        )
    }
}

/// One decoded full-disk segment of one band.
#[derive(Debug)]
pub struct Segment {
    pub band_number: u16,
    pub columns: usize,
    pub lines: usize,
    /// 1-based line number of this segment's first line within the full
    /// disk, at the band's native resolution.
    pub first_line: usize,
    pub segment_number: u8,
    pub total_segments: u8,
    pub calibration: Calibration,
    pub projection: Projection,
    /// Row-major raw counts, `columns * lines` long.
    pub counts: Vec<u16>,
}

struct DataInfo {
    columns: usize,
    lines: usize,
}

struct SegmentInfo {
    total_segments: u8,
    segment_number: u8,
    first_line: usize,
}

struct CalInfo {
    band_number: u16,
    calibration: Calibration,
}

pub fn parse(buf: &[u8]) -> Result<Segment, HsdError> {
    let r = Reader { buf };

    // Block 1 (basic information): validate magic-ish fields and learn how
    // many header blocks to walk.
    let first_block = r.u8(0)?;
    if first_block != 1 {
        return Err(HsdError::NotHsd(first_block));
    }
    let total_blocks = r.u16(3)?;
    if r.u8(5)? != 0 {
        return Err(HsdError::BigEndian);
    }

    let mut data_info: Option<DataInfo> = None;
    let mut proj_info: Option<Projection> = None;
    let mut cal_info: Option<CalInfo> = None;
    let mut seg_info: Option<SegmentInfo> = None;

    // Walk the chained header blocks; each starts with (u8 number, u16 length).
    let mut pos = 0usize;
    for _ in 0..total_blocks {
        let number = r.u8(pos)?;
        let length = r.u16(pos + 1)? as usize;
        match number {
            2 => {
                let bits = r.u16(pos + 3)?;
                if bits != 16 {
                    return Err(HsdError::BitsPerPixel(bits));
                }
                data_info = Some(DataInfo {
                    columns: r.u16(pos + 5)? as usize,
                    lines: r.u16(pos + 7)? as usize,
                });
            }
            3 => {
                proj_info = Some(Projection {
                    sub_lon_deg: r.f64(pos + 3)?,
                    cfac: r.u32(pos + 11)?,
                    lfac: r.u32(pos + 15)?,
                    coff: f64::from(r.f32(pos + 19)?),
                    loff: f64::from(r.f32(pos + 23)?),
                    distance_km: r.f64(pos + 27)?,
                    equatorial_radius_km: r.f64(pos + 35)?,
                    polar_radius_km: r.f64(pos + 43)?,
                });
            }
            5 => {
                let band_number = r.u16(pos + 3)?;
                let kind = if band_number <= 6 {
                    CalibrationKind::Reflective {
                        albedo_coeff: r.f64(pos + 35)?,
                    }
                } else {
                    CalibrationKind::Emissive {
                        wavelength_um: r.f64(pos + 5)?,
                        rad_to_bt: [r.f64(pos + 35)?, r.f64(pos + 43)?, r.f64(pos + 51)?],
                        speed_of_light: r.f64(pos + 83)?,
                        planck: r.f64(pos + 91)?,
                        boltzmann: r.f64(pos + 99)?,
                    }
                };
                cal_info = Some(CalInfo {
                    band_number,
                    calibration: Calibration {
                        error_count: r.u16(pos + 15)?,
                        outside_scan_count: r.u16(pos + 17)?,
                        gain: r.f64(pos + 19)?,
                        offset: r.f64(pos + 27)?,
                        kind,
                    },
                });
            }
            7 => {
                seg_info = Some(SegmentInfo {
                    total_segments: r.u8(pos + 3)?,
                    segment_number: r.u8(pos + 4)?,
                    first_line: r.u16(pos + 5)? as usize,
                });
            }
            _ => {}
        }
        pos += length;
    }

    let data = data_info.ok_or(HsdError::MissingBlock(2))?;
    let projection = proj_info.ok_or(HsdError::MissingBlock(3))?;
    let cal = cal_info.ok_or(HsdError::MissingBlock(5))?;
    let seg = seg_info.ok_or(HsdError::MissingBlock(7))?;

    let expected = data.columns * data.lines;
    let pixel_bytes = buf
        .get(pos..pos + expected * 2)
        .ok_or(HsdError::DataSize {
            expected,
            have: buf.len().saturating_sub(pos) / 2,
        })?;
    let counts: Vec<u16> = pixel_bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();

    Ok(Segment {
        band_number: cal.band_number,
        columns: data.columns,
        lines: data.lines,
        first_line: seg.first_line,
        segment_number: seg.segment_number,
        total_segments: seg.total_segments,
        calibration: cal.calibration,
        projection,
        counts,
    })
}
