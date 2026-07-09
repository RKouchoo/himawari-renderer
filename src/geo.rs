//! Per-pixel solar and viewing geometry for the geostationary full disk.
//!
//! The inverse normalized geostationary projection follows the JMA HSD user
//! guide (the same formulation as the CGMS LRIT/HRIT spec): pixel -> scan
//! angles -> intersection with the Earth ellipsoid -> geodetic lat/lon.
//! Solar declination and the equation of time use Spencer's Fourier fits,
//! which are accurate to well under a tenth of a degree — far below what a
//! quick-look brightness correction can show.

use std::f64::consts::PI;

use chrono::{DateTime, Datelike, Timelike, Utc};

use crate::hsd::Projection;

/// Cosines of the solar zenith and satellite view zenith angles at a pixel,
/// plus the glint angle: how closely the sun's specular reflection off a
/// horizontal surface aligns with the view direction (1 = mirror-perfect).
#[derive(Debug, Clone, Copy)]
pub struct Angles {
    pub cos_sun: f32,
    pub cos_view: f32,
    pub cos_glint: f32,
}

/// Scene-constant geometry: projection factors plus the solar ephemeris for
/// the observation time, ready for cheap per-pixel evaluation.
pub struct Geometry {
    sub_lon: f64,
    /// Radians of scan angle per pixel step.
    col_scale: f64,
    line_scale: f64,
    coff: f64,
    loff: f64,
    satellite_distance: f64,
    equatorial_radius: f64,
    polar_radius: f64,
    /// (equatorial radius / polar radius)^2
    req2_over_rpol2: f64,
    /// satellite_distance^2 - equatorial_radius^2
    sd_coeff: f64,
    sin_declination: f64,
    cos_declination: f64,
    /// UTC fractional hours plus the equation of time; adding the pixel's
    /// longitude (in hours) gives true solar time.
    base_solar_hours: f64,
    /// Unit vector toward the sun in the frame used throughout: x toward
    /// the sub-satellite longitude, z toward the north pole.
    sun_vector: [f64; 3],
}

impl Geometry {
    pub fn new(projection: &Projection, time: DateTime<Utc>) -> Self {
        let req = projection.equatorial_radius_km;
        let rpol = projection.polar_radius_km;
        let distance = projection.distance_km;

        // Solar position (Spencer 1971): fractional-year angle in radians.
        let hours = f64::from(time.hour())
            + f64::from(time.minute()) / 60.0
            + f64::from(time.second()) / 3600.0;
        let gamma = 2.0 * PI / 365.0 * (f64::from(time.ordinal()) - 1.0 + (hours - 12.0) / 24.0);
        let declination = 0.006918 - 0.399912 * gamma.cos() + 0.070257 * gamma.sin()
            - 0.006758 * (2.0 * gamma).cos()
            + 0.000907 * (2.0 * gamma).sin()
            - 0.002697 * (3.0 * gamma).cos()
            + 0.00148 * (3.0 * gamma).sin();
        let equation_of_time_min = 229.18
            * (0.000075 + 0.001868 * gamma.cos()
                - 0.032077 * gamma.sin()
                - 0.014615 * (2.0 * gamma).cos()
                - 0.040849 * (2.0 * gamma).sin());

        let base_solar_hours = hours + equation_of_time_min / 60.0;
        let sub_lon = projection.sub_lon_deg.to_radians();
        // Sub-solar longitude (where true solar time is noon), relative to
        // the sub-satellite longitude.
        let sub_solar_rel = ((12.0 - base_solar_hours) * 15.0).to_radians() - sub_lon;
        let sun_vector = [
            declination.cos() * sub_solar_rel.cos(),
            declination.cos() * sub_solar_rel.sin(),
            declination.sin(),
        ];

        Geometry {
            sub_lon,
            col_scale: (2f64.powi(-16) * f64::from(projection.cfac)).recip().to_radians(),
            line_scale: (2f64.powi(-16) * f64::from(projection.lfac)).recip().to_radians(),
            coff: projection.coff,
            loff: projection.loff,
            satellite_distance: distance,
            equatorial_radius: req,
            polar_radius: rpol,
            req2_over_rpol2: (req / rpol).powi(2),
            sd_coeff: distance * distance - req * req,
            sin_declination: declination.sin(),
            cos_declination: declination.cos(),
            base_solar_hours,
            sun_vector,
        }
    }

    /// Intersection of a pixel's line of sight with the Earth ellipsoid, in
    /// km, x-axis toward the sub-satellite point.
    fn surface_point(&self, col: f64, line: f64) -> Option<[f64; 3]> {
        // HSD pixel numbering is 1-based.
        let x = (col + 1.0 - self.coff) * self.col_scale;
        let y = (line + 1.0 - self.loff) * self.line_scale;
        let (sin_x, cos_x) = x.sin_cos();
        let (sin_y, cos_y) = y.sin_cos();

        let k = cos_y * cos_y + self.req2_over_rpol2 * sin_y * sin_y;
        let a = self.satellite_distance * cos_x * cos_y;
        let discriminant = a * a - k * self.sd_coeff;
        if discriminant < 0.0 {
            return None;
        }
        let slant = (a - discriminant.sqrt()) / k;
        Some([
            self.satellite_distance - slant * cos_x * cos_y,
            slant * sin_x * cos_y,
            -slant * sin_y,
        ])
    }

    /// Geometry at a (fractional, 0-based) pixel index of the grid this
    /// projection describes. None when the pixel looks past the Earth limb.
    pub fn angles(&self, col: f64, line: f64) -> Option<Angles> {
        let [px, py, pz] = self.surface_point(col, line)?;

        let pxy = (px * px + py * py).sqrt();
        let lat = (self.req2_over_rpol2 * pz / pxy).atan();
        let lon = py.atan2(px) + self.sub_lon;

        // Unit vector toward the satellite and the ellipsoid surface normal.
        let vx = self.satellite_distance - px;
        let vy = -py;
        let vz = -pz;
        let view_norm = (vx * vx + vy * vy + vz * vz).sqrt();
        let view = [vx / view_norm, vy / view_norm, vz / view_norm];
        let req2 = self.equatorial_radius * self.equatorial_radius;
        let rpol2 = self.polar_radius * self.polar_radius;
        let (nx, ny, nz) = (px / req2, py / req2, pz / rpol2);
        let n_norm = (nx * nx + ny * ny + nz * nz).sqrt();
        let normal = [nx / n_norm, ny / n_norm, nz / n_norm];

        let cos_view = normal[0] * view[0] + normal[1] * view[1] + normal[2] * view[2];

        // Solar zenith from true solar time at this longitude.
        let true_solar_hours = self.base_solar_hours + lon.to_degrees() / 15.0;
        let hour_angle = (true_solar_hours - 12.0) * PI / 12.0;
        let cos_sun =
            lat.sin() * self.sin_declination + lat.cos() * self.cos_declination * hour_angle.cos();

        // Glint: reflect the sun vector off the surface and compare with the
        // view direction.
        let sun = self.sun_vector;
        let sun_dot_n = sun[0] * normal[0] + sun[1] * normal[1] + sun[2] * normal[2];
        let mut cos_glint = 0.0;
        for axis in 0..3 {
            let reflected = 2.0 * sun_dot_n * normal[axis] - sun[axis];
            cos_glint += reflected * view[axis];
        }

        Some(Angles {
            cos_sun: cos_sun as f32,
            cos_view: cos_view as f32,
            cos_glint: cos_glint as f32,
        })
    }

    /// Where a cloud whose ground position is pixel (col, line) and whose top
    /// sits `height_km` above the surface *appears* in the image: the
    /// intersection of the satellite -> cloud-top ray with the ellipsoid.
    /// Sampling the IR grid there instead of at (col, line) undoes the
    /// parallax displacement of tall clouds viewed obliquely.
    pub fn apparent_position(&self, col: f64, line: f64, height_km: f64) -> Option<(f64, f64)> {
        let p = self.surface_point(col, line)?;
        let p_norm = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
        let lift = 1.0 + height_km / p_norm;
        // Ray from the satellite through the lifted cloud top.
        let d = [
            p[0] * lift - self.satellite_distance,
            p[1] * lift,
            p[2] * lift,
        ];

        // First intersection with the ellipsoid (x²+y²)/req² + z²/rpol² = 1.
        let req2 = self.equatorial_radius * self.equatorial_radius;
        let rpol2 = self.polar_radius * self.polar_radius;
        let a = (d[0] * d[0] + d[1] * d[1]) / req2 + d[2] * d[2] / rpol2;
        let b = 2.0 * self.satellite_distance * d[0] / req2;
        let c = self.satellite_distance * self.satellite_distance / req2 - 1.0;
        if b * b - 4.0 * a * c < 0.0 {
            return None; // the lifted cloud top peeks past the limb
        }

        // The apparent point lies along the same ray, so only the direction
        // matters: back to scan angles and pixel indices (inverse of
        // surface_point's convention).
        let norm = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        let dir = [d[0] / norm, d[1] / norm, d[2] / norm];
        let y = (-dir[2]).asin();
        let x = dir[1].atan2(-dir[0]);
        Some((
            x / self.col_scale + self.coff - 1.0,
            y / self.line_scale + self.loff - 1.0,
        ))
    }
}
