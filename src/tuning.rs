//! Every tunable in one place. Change a value here, `cargo build --release`,
//! and re-render (cached scenes re-render in seconds).
//!
//! The image pipeline these feed, in order:
//!   linear reflectance -> geometric corrections (sun angle, Rayleigh haze,
//!   glint, limb shade, twilight fade) -> display grade (gamma, contrast,
//!   saturation) -> 8-bit RGB. The combined product then blends the B13
//!   thermal band on top (night clouds + sandwich overlay).

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// Attempts per S3 object before giving up (exponential backoff between).
pub const DOWNLOAD_RETRIES: u32 = 3;
pub const CONNECT_TIMEOUT_SECS: u64 = 15;
pub const READ_TIMEOUT_SECS: u64 = 120;

/// Concurrent range-request connections per file. Files are already
/// downloaded in parallel with each other; this additionally splits each
/// file into byte ranges fetched on separate connections, so a single
/// object is not capped by S3's per-connection throughput. 1 disables
/// splitting.
pub const DOWNLOAD_CONNECTIONS_PER_FILE: u64 = 4;

/// Files smaller than this download on a single connection (the range
/// bookkeeping is not worth it for small objects).
pub const DOWNLOAD_SPLIT_MIN_BYTES: u64 = 8 * 1024 * 1024;

/// How many 10-minute slots to walk back from now when locating the latest
/// complete scene (36 = 6 hours).
pub const SCENE_LOOKBACK_SLOTS: usize = 36;

/// Seconds between bucket polls in --watch mode. New scenes land every
/// 10 minutes, so there is little point going below ~60.
pub const WATCH_POLL_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Timelapse
// ---------------------------------------------------------------------------

/// Default --downsample in timelapse mode (4 -> 2750x2750 frames).
pub const TIMELAPSE_DOWNSAMPLE: usize = 4;

/// Scenes downloaded concurrently ahead of the frame renderer. Downloads
/// dominate timelapse wall time, so this is close to a direct throughput
/// multiplier — at the cost of ~1 GB of RAM per scene in flight.
pub const TIMELAPSE_PREFETCH: usize = 2;

// ---------------------------------------------------------------------------
// Storm tracking (--follow-storm)
// ---------------------------------------------------------------------------

/// B13 brightness temperatures below this count as "storm cloud" for the
/// tracker; the weight of a pixel is how far below it sits.
pub const TRACK_COLD_THRESHOLD: f32 = 225.0;

/// Sliding-window size, in 2 km B13 pixels, used to seed the track on the
/// first frame: the window with the greatest cold-cloud weight wins.
pub const TRACK_SEED_WINDOW: usize = 256;

/// Seeding ignores pixels beyond this normalized disk radius, excluding
/// limb-cooling artifacts and the wintertime polar surface.
pub const TRACK_SEED_MAX_RADIUS: f64 = 0.8;

/// Frame-to-frame search radius around the previous position, in 2 km
/// pixels (150 px = 300 km; cyclones drift a few km per 10-minute slot).
pub const TRACK_SEARCH_RADIUS: usize = 150;

/// Minimum total cold weight (kelvin-below-threshold summed over pixels)
/// for a fix; below this the tracker holds its previous position.
pub const TRACK_MIN_WEIGHT: f64 = 500.0;

/// Blend factor for each new fix (1 = jump straight to it, small = heavy
/// smoothing). Keeps the crop from jittering.
pub const TRACK_SMOOTHING: f64 = 0.35;

/// x264 constant rate factor for the encoded video (lower = better/larger;
/// 18 is visually lossless for most content).
pub const VIDEO_CRF: u32 = 18;

// ---------------------------------------------------------------------------
// True-color recipe
// ---------------------------------------------------------------------------

/// Hybrid-green blend: AHI's 0.51 um green band under-represents vegetation,
/// so a fraction of the 0.86 um near-IR band is mixed in (Miller et al. 2016;
/// same fractions the standard AHI true-color recipe uses).
pub const GREEN_FRACTION: f32 = 0.85;
pub const NIR_FRACTION: f32 = 0.15;

// ---------------------------------------------------------------------------
// Display grade
// ---------------------------------------------------------------------------

/// Display gamma applied to reflectance before quantizing to 8 bits.
pub const GAMMA: f32 = 2.2;

/// Strength of the S-shaped contrast curve applied after gamma (0 disables).
/// Deepens shadows and shapes cloud highlights without clipping either end.
pub const CONTRAST: f32 = 0.48;

/// Chroma multiplier applied in display space. Atmospheric scattering
/// dilutes every channel toward haze gray; pushing each channel away from
/// its luma restores the kind of color the enhanced quick-looks show.
/// 1.0 disables the boost.
pub const SATURATION: f32 = 1.35;

// ---------------------------------------------------------------------------
// Geometric corrections (sun angle, Rayleigh haze, glint, limb)
// ---------------------------------------------------------------------------

/// Nadir-equivalent Rayleigh path radiance per channel (R, G, B), expressed
/// as reflectance. Atmospheric scattering lays a wavelength-dependent
/// gray-blue veil over the scene (strongest in blue); each pixel subtracts
/// this scaled by its air mass — the longer the light path through the
/// atmosphere, the more veil is removed.
pub const HAZE_NADIR: [f32; 3] = [0.011, 0.022, 0.040];

/// View-path air mass (secant of the view zenith), capped so the correction
/// stays stable right at the limb. The solar path deliberately does not
/// enter the air mass: sun-angle normalization already accounts for
/// illumination, and folding the solar secant into the haze term makes the
/// subtraction explode at the terminator.
pub const MAX_VIEW_AIRMASS: f32 = 5.5;
pub const MIN_COS_VIEW: f32 = 0.16;

/// Fraction of the full 1/cos(solar zenith) brightness normalization to
/// apply. 1.0 flattens the disk into a map-like product with no shading at
/// all; 0 keeps the raw lighting with a dim, smeared terminator. 0.55
/// brightens low-sun regions enough to stay readable while leaving real
/// illumination shading on clouds and the sphere. This is the main
/// depth-versus-evenness dial.
pub const SUN_CORRECTION: f32 = 0.55;
/// Floor for the normalization divisor: caps the low-sun boost at ~5.6x so
/// twilight regions do not blow out into a bright rim.
pub const MIN_COS_SUN: f32 = 0.10;

/// Twilight fade, in cos(solar zenith): full daylight above DAYLIGHT_FULL,
/// black below DAYLIGHT_ZERO (just past the horizon, so the terminator
/// keeps a soft civil-twilight edge instead of a hard cut).
pub const DAYLIGHT_FULL: f32 = 0.12;
pub const DAYLIGHT_ZERO: f32 = -0.03;

/// Gentle brightness rolloff at the extreme limb, where even the capped
/// haze subtraction cannot keep up with the diverging slant path. Shading
/// starts once cos(view zenith) drops below LIMB_SHADE_START and bottoms
/// out at LIMB_SHADE_FLOOR of full brightness right at the edge.
pub const LIMB_SHADE_START: f32 = 0.45;
pub const LIMB_SHADE_FLOOR: f32 = 0.22;

/// Sun-glint softening: within GLINT_COS_EDGE of the specular direction
/// (cos 0.955 is roughly a 17-degree cone) up to GLINT_STRENGTH of white
/// veil is subtracted, taming the silver mirror patch on the ocean without
/// visibly denting bright clouds.
pub const GLINT_COS_EDGE: f32 = 0.955;
pub const GLINT_STRENGTH: f32 = 0.10;

// ---------------------------------------------------------------------------
// IR enhancement (CLUT)
// ---------------------------------------------------------------------------

/// Piecewise-linear color lookup tables over brightness temperature in
/// kelvin, selected with `--clut-style`. Points must be sorted by ascending
/// temperature; colors between points are linearly interpolated, and
/// temperatures beyond either end clamp to the end color.
///
/// `convection` (the default, and the table the combined product's sandwich
/// overlay always uses): warm scene temperatures render as inverted
/// grayscale (warm surface dark, cold cloud bright); below 240 K the ramp
/// runs blue -> green -> yellow -> red so convective cloud tops stand out,
/// scaled so red is reached at the temperatures the coldest real tops
/// actually hit (~195-210 K) rather than only in theory.
pub const CLUT_CONVECTION: &[(f32, [f32; 3])] = &[
    (182.0, [140.0, 0.0, 0.0]),     // extreme overshooting tops: deep red
    (204.0, [255.0, 0.0, 0.0]),     // red — covers the bulk of a cold CDO
    (214.0, [255.0, 255.0, 0.0]),   // yellow
    (226.0, [0.0, 200.0, 0.0]),     // green
    (235.0, [0.0, 0.0, 255.0]),     // pure blue where the sandwich fades in
    (240.0, [80.0, 160.0, 255.0]),  // light blue, end of the cold ramp
    (240.1, [220.0, 220.0, 220.0]), // jump to the grayscale segment
    (330.0, [0.0, 0.0, 0.0]),       // warm surface: black
];

/// Plain inverted grayscale — the classic clean-IR display: cold cloud
/// bright, warm surface dark, no color anywhere.
pub const CLUT_GRAYSCALE: &[(f32, [f32; 3])] = &[
    (180.0, [255.0, 255.0, 255.0]),
    (320.0, [0.0, 0.0, 0.0]),
];

/// Full-scene rainbow: every temperature gets a hue, so surface gradients
/// show as well as cloud tops (the retro "colorized IR" look).
pub const CLUT_RAINBOW: &[(f32, [f32; 3])] = &[
    (190.0, [255.0, 255.0, 255.0]), // coldest tops: white
    (200.0, [255.0, 0.0, 255.0]),   // magenta
    (212.0, [255.0, 0.0, 0.0]),     // red
    (224.0, [255.0, 255.0, 0.0]),   // yellow
    (236.0, [0.0, 220.0, 0.0]),     // green
    (248.0, [0.0, 255.0, 255.0]),   // cyan
    (262.0, [0.0, 0.0, 255.0]),     // blue
    (280.0, [20.0, 20.0, 90.0]),    // navy
    (320.0, [0.0, 0.0, 0.0]),       // warm surface: black
];

/// Water-vapor palette, tuned for the B08-B10 6-7 um channels: dry
/// descending air (warm brightness temperatures) in browns and oranges,
/// moist mid-levels in gray-white, cold convective tops in blues and teal.
pub const CLUT_WATER_VAPOR: &[(f32, [f32; 3])] = &[
    (185.0, [255.0, 0.0, 255.0]),   // extreme overshooting tops: magenta
    (195.0, [0.0, 220.0, 180.0]),   // teal
    (205.0, [0.0, 130.0, 255.0]),   // blue
    (215.0, [120.0, 220.0, 255.0]), // light blue
    (228.0, [255.0, 255.0, 255.0]), // white, moist upper troposphere
    (238.0, [180.0, 180.0, 180.0]), // gray
    (246.0, [235.0, 200.0, 160.0]), // tan
    (258.0, [200.0, 100.0, 30.0]),  // orange, drying
    (270.0, [70.0, 30.0, 0.0]),     // dark brown, very dry
    (330.0, [40.0, 15.0, 0.0]),
];

/// Brightness temperatures below this are not physical for cloud tops; they
/// come from sensor noise in space pixels, which would otherwise light up
/// the cold end of the CLUT around the limb.
pub const SPACE_BT_CUTOFF: f32 = 160.0;

// ---------------------------------------------------------------------------
// Combined product (true color + B13)
// ---------------------------------------------------------------------------

/// Night-side IR clouds fade in as the sun sets (the complement of the
/// daylight factor) and ramp from invisible at NIGHT_IR_WARM (surface
/// temperature, no cloud) to full white at NIGHT_IR_COLD.
pub const NIGHT_IR_WARM: f32 = 290.0;
pub const NIGHT_IR_COLD: f32 = 200.0;

/// Sandwich overlay: cold cloud tops get their CLUT color starting at
/// SANDWICH_START, reaching SANDWICH_ALPHA opacity SANDWICH_RAMP kelvin
/// colder — day and night alike. SANDWICH_START sits below the CLUT's 240 K
/// color threshold on purpose: it filters out marginal pixels that barely
/// enter the cold ramp, so only genuinely convective tops are painted, and
/// the ones that qualify are painted vividly.
pub const SANDWICH_START: f32 = 235.0;
pub const SANDWICH_RAMP: f32 = 4.0;
pub const SANDWICH_ALPHA: f32 = 0.85;

/// The sandwich overlay fades out beyond this normalized disk radius and is
/// gone LIMB_FADE_WIDTH later. At grazing view angles the long atmospheric
/// path makes B13 read cold ("limb cooling"), which would paint a false
/// colored ring around the edge of the disk.
pub const LIMB_FADE_START: f32 = 0.965;
pub const LIMB_FADE_WIDTH: f32 = 0.02;

/// Cloud-top height estimation from B13 brightness temperature: a fixed
/// lapse rate below a reference surface temperature. Used both by the
/// combined product's parallax correction and by the --cloud-height render.
pub const PARALLAX_REF_TEMP: f32 = 295.0;
pub const PARALLAX_LAPSE_K_PER_KM: f32 = 6.5;
pub const PARALLAX_MAX_HEIGHT_KM: f32 = 16.0;
/// Skip the (two-projection) parallax correction for shallow cloud.
pub const PARALLAX_MIN_HEIGHT_KM: f32 = 1.0;

// ---------------------------------------------------------------------------
// Cloud-top height render
// ---------------------------------------------------------------------------

/// Pixels whose estimated top sits below this count as cloud-free in the
/// --cloud-height render (warm surfaces read as ~0 height).
pub const CLOUD_MIN_HEIGHT_KM: f32 = 0.5;

/// What cloud-free (and space) pixels render as: near-black, so the colored
/// cloud decks float on a dark disk.
pub const HEIGHT_CLEAR_RGB: [u8; 3] = [10, 10, 14];

/// Hypsometric palette over cloud-top height in km, read like a relief map:
/// green lowlands through yellow and orange foothills to brown high ground
/// and white at the tropopause. Points sorted ascending; linear blend
/// between, clamped at the ends.
pub const HEIGHT_CLUT: &[(f32, [f32; 3])] = &[
    (0.5, [30.0, 70.0, 35.0]),      // shallow stratus: dark green
    (2.0, [70.0, 150.0, 60.0]),     // low cloud: green
    (4.0, [160.0, 190.0, 80.0]),    // yellow-green
    (6.0, [220.0, 200.0, 100.0]),   // mid-level: tan
    (8.0, [230.0, 150.0, 60.0]),    // orange
    (10.0, [200.0, 90.0, 40.0]),    // red-brown
    (12.0, [150.0, 55.0, 30.0]),    // deep convection: dark brown
    (14.0, [220.0, 220.0, 230.0]),  // near the tropopause: pale
    (16.0, [255.0, 255.0, 255.0]),  // overshooting tops: white
];
