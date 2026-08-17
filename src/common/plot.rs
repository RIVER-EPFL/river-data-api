//! PNG chart rendering for Telegram plot replies.
//!
//! Deliberately free of the database, `AppState` and every bot type: the caller assembles a
//! [`PlotSpec`] and this module turns it into bytes. That keeps `PlotSpec` `Send + 'static`, which
//! is what lets the render run under `spawn_blocking`, and it means an HTTP `plot.png` route would
//! be a router line rather than a refactor.
//!
//! Fonts are embedded rather than resolved through fontconfig: the runtime image is
//! `debian:bookworm-slim`, which ships no font files at all.
//!
//! Colours come from the dashboard's design tokens (`river-data-ui/src/lib/charts/tokens.ts`) so a
//! chart in Telegram and the same series in the browser read as one system.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use plotters::prelude::*;
use plotters::style::text_anchor::{HPos, Pos, VPos};
use plotters::style::{FontStyle, register_font};

/// DejaVu, not Inter: parameter units here include `µS/cm` and `°C`, and DejaVu guarantees those
/// glyphs. A missing glyph renders as a blank box, which is worse than an unfashionable typeface.
const FONT_REGULAR: &[u8] = include_bytes!("../../assets/fonts/DejaVuSans.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../../assets/fonts/DejaVuSans-Bold.ttf");

/// The family name every text style below refers to.
pub const FONT_FAMILY: &str = "sans";

/// Telegram downscales photos whose longest side exceeds ~1280px, so rendering at exactly 1280
/// means no resample and crisp text.
pub const DEFAULT_WIDTH: u32 = 1280;
pub const DEFAULT_HEIGHT: u32 = 720;

// Design tokens, mirrored from the dashboard.
const C_TEXT: RGBColor = RGBColor(0x1B, 0x23, 0x30);
const C_MUTED: RGBColor = RGBColor(0x5A, 0x64, 0x72);
const C_GRID: RGBColor = RGBColor(0xEA, 0xEC, 0xEF);
const C_SURFACE: RGBColor = RGBColor(0xFF, 0xFF, 0xFF);
const C_DIVIDER: RGBColor = RGBColor(0xE2, 0xE5, 0xEA);
/// Okabe-Ito blue, the dashboard's first data-viz series colour.
const C_SERIES: RGBColor = RGBColor(0x00, 0x72, 0xB2);
const C_WARNING: RGBColor = RGBColor(0xCA, 0x8A, 0x04);
const C_ALARM: RGBColor = RGBColor(0xC6, 0x28, 0x28);
/// Brand accent, used for annotation bands.
const C_ANNOTATION: RGBColor = RGBColor(0xC7, 0x77, 0x00);

/// At most this many annotation bands are drawn; the caption reports any excess.
pub const MAX_ANNOTATION_BANDS: usize = 20;
/// Beyond this many points, per-point markers become a smear and are dropped.
const MARKER_LIMIT: usize = 200;

static FONT_INIT: OnceLock<Result<(), String>> = OnceLock::new();

/// Register the embedded fonts with plotters, once per process.
///
/// `register_font` is global and idempotent per (family, style), so this is wrapped in a
/// `OnceLock` and every render calls it. A failure here means the vendored file is corrupt, which
/// a unit test catches at CI rather than in production.
pub fn ensure_fonts() -> Result<(), String> {
    FONT_INIT
        .get_or_init(|| {
            register_font(FONT_FAMILY, FontStyle::Normal, FONT_REGULAR)
                .map_err(|_| "failed to register DejaVuSans".to_string())?;
            register_font(FONT_FAMILY, FontStyle::Bold, FONT_BOLD)
                .map_err(|_| "failed to register DejaVuSans-Bold".to_string())
        })
        .clone()
}

/// A time-ranged note to shade behind the series.
#[derive(Debug, Clone)]
pub struct AnnotationBand {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub text: String,
}

/// The four resolved threshold bounds, drawn as dashed horizontal limit lines.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThresholdLines {
    pub warning_min: Option<f64>,
    pub warning_max: Option<f64>,
    pub alarm_min: Option<f64>,
    pub alarm_max: Option<f64>,
}

impl ThresholdLines {
    fn lines(&self) -> impl Iterator<Item = (f64, RGBColor, &'static str)> + '_ {
        [
            (self.alarm_min, C_ALARM, "alarm"),
            (self.alarm_max, C_ALARM, "alarm"),
            (self.warning_min, C_WARNING, "warn"),
            (self.warning_max, C_WARNING, "warn"),
        ]
        .into_iter()
        .filter_map(|(v, c, l)| v.map(|v| (v, c, l)))
    }
}

/// Everything needed to draw one chart. Owned throughout, so it crosses a `spawn_blocking`
/// boundary without borrowing.
#[derive(Debug, Clone)]
pub struct PlotSpec {
    /// e.g. "Verbier: Depth"
    pub title: String,
    /// e.g. "last 7 days · hourly means · 168 points"
    pub subtitle: String,
    /// e.g. "Depth (mm)"
    pub y_label: String,
    pub points: Vec<(DateTime<Utc>, f64)>,
    /// Per-bucket extrema drawn as a soft band under the line. Empty at the raw tier.
    pub envelope: Vec<(DateTime<Utc>, f64, f64)>,
    pub thresholds: ThresholdLines,
    pub annotations: Vec<AnnotationBand>,
    /// A gap wider than this starts a new line segment rather than bridging the outage.
    pub gap_seconds: i64,
    pub width: u32,
    pub height: u32,
}

impl PlotSpec {
    /// A spec with the standard dimensions and no overlays.
    #[must_use]
    pub fn new(title: String, subtitle: String, y_label: String) -> Self {
        Self {
            title,
            subtitle,
            y_label,
            points: Vec::new(),
            envelope: Vec::new(),
            thresholds: ThresholdLines::default(),
            annotations: Vec::new(),
            gap_seconds: i64::MAX,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        }
    }
}

#[derive(Debug)]
pub enum PlotError {
    /// Nothing to draw. The caller should say so in words rather than send an empty chart.
    NoData,
    Render(String),
}

impl std::fmt::Display for PlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlotError::NoData => write!(f, "no data to plot"),
            PlotError::Render(e) => write!(f, "render failed: {e}"),
        }
    }
}

/// A y-range that pads additively.
///
/// The legacy R bot used `c(min * 0.95, max * 1.05)`, which inverts on an all-negative series
/// (`-10 * 0.95` is *above* `-10`), clips a zero-crossing series, and collapses to a degenerate
/// range when every value is equal. Water temperature near zero hits all three.
#[must_use]
fn y_range(lo: f64, hi: f64) -> (f64, f64) {
    let span = hi - lo;
    let pad = if span > 0.0 {
        span * 0.08
    } else {
        (hi.abs() * 0.05).max(1.0)
    };
    (lo - pad, hi + pad)
}

/// Split into runs broken wherever consecutive points are more than `gap_seconds` apart, so an
/// outage shows as a gap instead of a straight line drawn through it.
fn segments(points: &[(DateTime<Utc>, f64)], gap_seconds: i64) -> Vec<Vec<(DateTime<Utc>, f64)>> {
    let mut out: Vec<Vec<(DateTime<Utc>, f64)>> = Vec::new();
    let mut current: Vec<(DateTime<Utc>, f64)> = Vec::new();
    for &(t, v) in points {
        if let Some(&(prev_t, _)) = current.last()
            && (t - prev_t).num_seconds() > gap_seconds
        {
            out.push(std::mem::take(&mut current));
        }
        current.push((t, v));
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Whether a threshold is close enough to the data to be worth drawing.
///
/// Depth alarms at 2000 mm while a healthy series sits at 0-100. Including that bound in the
/// y-range unconditionally would flatten the data into a line along the bottom of the chart, so a
/// far-away limit is simply not drawn and the caller says so in the caption.
fn threshold_in_view(value: f64, lo: f64, hi: f64) -> bool {
    let span = (hi - lo).abs().max(f64::EPSILON);
    value >= lo - 3.0 * span && value <= hi + 3.0 * span
}

/// Render `spec` to PNG bytes.
///
/// CPU-bound and fully synchronous; call it under `spawn_blocking` so it cannot stall the async
/// runtime (the bot poller handles updates serially).
pub fn render_png(spec: &PlotSpec) -> Result<Vec<u8>, PlotError> {
    ensure_fonts().map_err(PlotError::Render)?;

    let finite: Vec<(DateTime<Utc>, f64)> = spec
        .points
        .iter()
        .filter(|(_, v)| v.is_finite())
        .copied()
        .collect();
    if finite.is_empty() {
        return Err(PlotError::NoData);
    }

    let x_min = finite.first().map(|p| p.0).unwrap_or_else(Utc::now);
    let x_max = finite.last().map(|p| p.0).unwrap_or(x_min);
    // A single point, or several sharing one timestamp, would make a zero-width domain that
    // plotters cannot build a range from.
    let x_max = if x_max <= x_min {
        x_min + chrono::Duration::minutes(1)
    } else {
        x_max
    };

    let mut data_lo = f64::INFINITY;
    let mut data_hi = f64::NEG_INFINITY;
    for &(_, v) in &finite {
        data_lo = data_lo.min(v);
        data_hi = data_hi.max(v);
    }
    for &(_, lo, hi) in &spec.envelope {
        if lo.is_finite() {
            data_lo = data_lo.min(lo);
        }
        if hi.is_finite() {
            data_hi = data_hi.max(hi);
        }
    }
    for (v, _, _) in spec.thresholds.lines() {
        if v.is_finite() && threshold_in_view(v, data_lo, data_hi) {
            data_lo = data_lo.min(v);
            data_hi = data_hi.max(v);
        }
    }
    let (y_lo, y_hi) = y_range(data_lo, data_hi);

    let mut buf = vec![0u8; (spec.width * spec.height * 3) as usize];
    {
        let root =
            BitMapBackend::with_buffer(&mut buf, (spec.width, spec.height)).into_drawing_area();
        root.fill(&C_SURFACE).map_err(render_err)?;

        let root = root.margin(12, 12, 12, 12);
        let (header, body) = root.split_vertically(64);

        header
            .draw_text(
                &spec.title,
                &(FONT_FAMILY, 26, FontStyle::Bold).into_text_style(&header).color(&C_TEXT),
                (4, 4),
            )
            .map_err(render_err)?;
        header
            .draw_text(
                &spec.subtitle,
                &(FONT_FAMILY, 15).into_text_style(&header).color(&C_MUTED),
                (4, 38),
            )
            .map_err(render_err)?;

        let mut chart = ChartBuilder::on(&body)
            .margin_right(16)
            .x_label_area_size(44)
            .y_label_area_size(72)
            .build_cartesian_2d(x_min..x_max, y_lo..y_hi)
            .map_err(render_err)?;

        chart
            .configure_mesh()
            .light_line_style(C_GRID.mix(0.55))
            .bold_line_style(C_GRID)
            .axis_style(C_DIVIDER)
            .label_style((FONT_FAMILY, 12).into_font().color(&C_MUTED))
            .y_desc(&spec.y_label)
            .x_labels(8)
            .y_labels(6)
            .x_label_formatter(&|t: &DateTime<Utc>| format_tick(*t, x_min, x_max))
            .draw()
            .map_err(render_err)?;

        // Annotation bands sit behind everything: they are context, not data.
        for band in spec.annotations.iter().take(MAX_ANNOTATION_BANDS) {
            let start = band.start.max(x_min);
            let end = band.end.min(x_max);
            if end <= start {
                continue;
            }
            chart
                .draw_series(std::iter::once(Rectangle::new(
                    [(start, y_lo), (end, y_hi)],
                    C_ANNOTATION.mix(0.12).filled(),
                )))
                .map_err(render_err)?;
            for edge in [start, end] {
                chart
                    .draw_series(LineSeries::new(
                        [(edge, y_lo), (edge, y_hi)],
                        C_ANNOTATION.mix(0.35).stroke_width(1),
                    ))
                    .map_err(render_err)?;
            }
        }

        // The min/max envelope: the honest way to show what bucketing hid.
        if !spec.envelope.is_empty() {
            let upper = spec.envelope.iter().map(|&(t, _, hi)| (t, hi));
            let lower = spec.envelope.iter().rev().map(|&(t, lo, _)| (t, lo));
            let polygon: Vec<_> = upper.chain(lower).collect();
            chart
                .draw_series(std::iter::once(Polygon::new(
                    polygon,
                    C_SERIES.mix(0.15).filled(),
                )))
                .map_err(render_err)?;
        }

        // Thresholds as thin dashed limit lines, matching how the dashboard draws them. Full
        // shading is reserved for time-period severity bands and would drown the data here.
        for (value, colour, label) in spec.thresholds.lines() {
            if !value.is_finite() || value < y_lo || value > y_hi {
                continue;
            }
            chart
                .draw_series(DashedLineSeries::new(
                    [(x_min, value), (x_max, value)],
                    5,
                    4,
                    colour.mix(0.75).stroke_width(1),
                ))
                .map_err(render_err)?;
            // Left-anchored: the stats box occupies the top right, and a right-anchored label
            // collides with it whenever the upper alarm bound is near the top of the range.
            chart
                .draw_series(std::iter::once(Text::new(
                    format!("{label} {}", trim_number(value)),
                    (x_min, value),
                    (FONT_FAMILY, 11).into_font().color(&colour).pos(Pos::new(
                        HPos::Left,
                        VPos::Bottom,
                    )),
                )))
                .map_err(render_err)?;
        }

        for segment in segments(&finite, spec.gap_seconds) {
            chart
                .draw_series(LineSeries::new(
                    segment.iter().copied(),
                    C_SERIES.stroke_width(2),
                ))
                .map_err(render_err)?;
        }
        if finite.len() <= MARKER_LIMIT {
            chart
                .draw_series(
                    finite
                        .iter()
                        .map(|&(t, v)| Circle::new((t, v), 2, C_SERIES.filled())),
                )
                .map_err(render_err)?;
        }

        draw_stats_box(&body, &finite).map_err(render_err)?;

        root.present().map_err(render_err)?;
    }

    encode_png(&buf, spec.width, spec.height)
}

/// Current / Mean / Min / Max / n, kept from the legacy bot because it is the part people read.
fn draw_stats_box<DB: DrawingBackend>(
    area: &DrawingArea<DB, plotters::coord::Shift>,
    points: &[(DateTime<Utc>, f64)],
) -> Result<(), String>
where
    DB::ErrorType: 'static,
{
    let n = points.len();
    let current = points.last().map(|p| p.1).unwrap_or(f64::NAN);
    let sum: f64 = points.iter().map(|p| p.1).sum();
    let mean = sum / n as f64;
    let min = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
    let max = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);

    let lines = [
        format!("Current  {}", trim_number(current)),
        format!("Mean     {}", trim_number(mean)),
        format!("Min      {}", trim_number(min)),
        format!("Max      {}", trim_number(max)),
        format!("n        {n}"),
    ];

    let (w, _) = area.dim_in_pixel();
    let box_w = 150i32;
    let box_h = 96i32;
    let x0 = w as i32 - box_w - 24;
    let y0 = 12i32;

    area.draw(&Rectangle::new(
        [(x0, y0), (x0 + box_w, y0 + box_h)],
        C_SURFACE.mix(0.88).filled(),
    ))
    .map_err(|e| e.to_string())?;
    area.draw(&Rectangle::new(
        [(x0, y0), (x0 + box_w, y0 + box_h)],
        C_DIVIDER.stroke_width(1),
    ))
    .map_err(|e| e.to_string())?;

    for (i, line) in lines.iter().enumerate() {
        area.draw_text(
            line,
            &(FONT_FAMILY, 11).into_text_style(area).color(&C_MUTED),
            (x0 + 10, y0 + 9 + i as i32 * 17),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Tick labels scale with the window: clock time for a short one, dates for a long one.
fn format_tick(t: DateTime<Utc>, x_min: DateTime<Utc>, x_max: DateTime<Utc>) -> String {
    let span_hours = (x_max - x_min).num_hours();
    if span_hours <= 48 {
        t.format("%H:%M").to_string()
    } else if span_hours <= 24 * 120 {
        t.format("%d %b").to_string()
    } else {
        t.format("%b %Y").to_string()
    }
}

/// Two decimals, without a trailing `.00` on a whole number.
fn trim_number(v: f64) -> String {
    if !v.is_finite() {
        return "–".to_string();
    }
    if (v - v.round()).abs() < 1e-9 && v.abs() < 1e15 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.2}")
    }
}

fn render_err<E: std::fmt::Display>(e: E) -> PlotError {
    PlotError::Render(e.to_string())
}

/// Encode the RGB buffer as PNG.
fn encode_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>, PlotError> {
    let mut out = Vec::new();
    {
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        use image::ImageEncoder;
        encoder
            .write_image(rgb, width, height, image::ColorType::Rgb8)
            .map_err(render_err)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn spec_with(points: Vec<(DateTime<Utc>, f64)>) -> PlotSpec {
        let mut s = PlotSpec::new(
            "Verbier: Depth".to_string(),
            "last 7 days · hourly means".to_string(),
            "Depth (mm)".to_string(),
        );
        s.points = points;
        s
    }

    fn series(n: i64) -> Vec<(DateTime<Utc>, f64)> {
        let t0 = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        (0..n)
            .map(|i| {
                (
                    t0 + Duration::minutes(i * 5),
                    100.0 + 20.0 * ((i as f64) / 9.0).sin(),
                )
            })
            .collect()
    }

    #[test]
    fn embedded_fonts_register() {
        assert!(
            ensure_fonts().is_ok(),
            "vendored DejaVu fonts must load; a failure here means assets/fonts is corrupt"
        );
    }

    #[test]
    fn embedded_fonts_are_not_empty() {
        assert!(FONT_REGULAR.len() > 100_000, "DejaVuSans.ttf looks truncated");
        assert!(FONT_BOLD.len() > 100_000, "DejaVuSans-Bold.ttf looks truncated");
    }

    // The legacy `c(min * 0.95, max * 1.05)` is wrong in three distinct ways; each gets a test.
    #[test]
    fn y_range_does_not_invert_on_negative_series() {
        let (lo, hi) = y_range(-10.0, -2.0);
        assert!(lo < -10.0, "lower bound must sit below the minimum, got {lo}");
        assert!(hi > -2.0, "upper bound must sit above the maximum, got {hi}");
    }

    #[test]
    fn y_range_frames_a_zero_crossing_series() {
        let (lo, hi) = y_range(-5.0, 5.0);
        assert!(lo < -5.0 && hi > 5.0, "got {lo}..{hi}");
    }

    #[test]
    fn y_range_gives_a_constant_series_a_visible_span() {
        let (lo, hi) = y_range(3.0, 3.0);
        assert!(hi - lo > 0.0, "a flat series still needs a drawable range");
    }

    #[test]
    fn far_thresholds_are_excluded_from_view() {
        // Depth alarms at 2000 while the data sits at 0-100.
        assert!(!threshold_in_view(2000.0, 0.0, 100.0));
        assert!(threshold_in_view(120.0, 0.0, 100.0));
    }

    #[test]
    fn segments_break_on_a_gap() {
        let t0 = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let pts = vec![
            (t0, 1.0),
            (t0 + Duration::minutes(5), 2.0),
            (t0 + Duration::hours(6), 3.0),
            (t0 + Duration::hours(6) + Duration::minutes(5), 4.0),
        ];
        let segs = segments(&pts, 1_800);
        assert_eq!(segs.len(), 2, "a six-hour hole must break the line");
        assert_eq!(segs[0].len(), 2);
        assert_eq!(segs[1].len(), 2);
    }

    #[test]
    fn segments_stay_whole_without_a_gap() {
        let pts = series(20);
        assert_eq!(segments(&pts, 1_800).len(), 1);
    }

    #[test]
    fn renders_a_png_of_the_expected_size() {
        let png = render_png(&spec_with(series(500))).expect("render");
        assert_eq!(
            &png[..8],
            b"\x89PNG\r\n\x1a\n",
            "output must be a real PNG"
        );
        assert!(png.len() > 1_000, "suspiciously small: {} bytes", png.len());
        assert!(
            png.len() < 10 * 1024 * 1024,
            "over Telegram's 10MB photo cap: {} bytes",
            png.len()
        );
        let decoded = image::load_from_memory(&png).expect("decodable");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (DEFAULT_WIDTH, DEFAULT_HEIGHT)
        );
    }

    #[test]
    fn renders_with_every_overlay() {
        let mut spec = spec_with(series(300));
        spec.thresholds = ThresholdLines {
            warning_min: Some(85.0),
            warning_max: Some(115.0),
            alarm_min: Some(70.0),
            alarm_max: Some(130.0),
        };
        let t0 = spec.points[0].0;
        spec.annotations = vec![
            AnnotationBand {
                start: t0 + Duration::minutes(100),
                end: t0 + Duration::minutes(300),
                text: "probe fouled".to_string(),
            },
            AnnotationBand {
                // Starts before the window and is still open: must clip, not vanish.
                start: t0 - Duration::days(2),
                end: t0 + Duration::minutes(50),
                text: "bank works".to_string(),
            },
        ];
        spec.envelope = spec
            .points
            .iter()
            .map(|&(t, v)| (t, v - 6.0, v + 6.0))
            .collect();
        spec.gap_seconds = 1_800;
        let png = render_png(&spec).expect("render with overlays");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn empty_series_is_no_data_not_an_empty_chart() {
        assert!(matches!(
            render_png(&spec_with(vec![])),
            Err(PlotError::NoData)
        ));
    }

    #[test]
    fn non_finite_values_are_dropped_not_drawn() {
        // A bad calibration formula can produce NaN; it must not take the whole chart down.
        let t0 = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let pts = vec![
            (t0, 1.0),
            (t0 + Duration::minutes(5), f64::NAN),
            (t0 + Duration::minutes(10), 3.0),
            (t0 + Duration::minutes(15), f64::INFINITY),
        ];
        let png = render_png(&spec_with(pts)).expect("render");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn all_non_finite_is_no_data() {
        let t0 = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let pts = vec![(t0, f64::NAN), (t0 + Duration::minutes(5), f64::NAN)];
        assert!(matches!(render_png(&spec_with(pts)), Err(PlotError::NoData)));
    }

    #[test]
    fn a_single_point_still_renders() {
        let t0 = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let png = render_png(&spec_with(vec![(t0, 42.0)])).expect("one point is drawable");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn a_flat_series_still_renders() {
        let t0 = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let pts: Vec<_> = (0..50)
            .map(|i| (t0 + Duration::minutes(i * 5), 7.0))
            .collect();
        let png = render_png(&spec_with(pts)).expect("a constant series is drawable");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn unicode_units_render() {
        // The reason DejaVu is vendored: µ and ° must not come out as blank boxes.
        let mut spec = spec_with(series(30));
        spec.y_label = "Conductivity (µS/cm) · 20 °C".to_string();
        spec.title = "Les Dailles: Conductivité".to_string();
        let png = render_png(&spec).expect("render");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }

    /// Writes a sample chart to `$RIVER_PLOT_DUMP` for eyeballing. Ignored by default: it asserts
    /// nothing a machine can check, and the point is to look at it.
    #[test]
    #[ignore = "visual check: cargo test --lib dump_sample -- --ignored"]
    fn dump_sample() {
        let path = std::env::var("RIVER_PLOT_DUMP").unwrap_or_else(|_| "/tmp/plot.png".to_string());
        let mut spec = spec_with(series(2016));
        spec.title = "Les Dailles: Conductivity".to_string();
        spec.subtitle = "last 7 days · hourly means · 168 points".to_string();
        spec.y_label = "Conductivity (µS/cm)".to_string();
        spec.thresholds = ThresholdLines {
            warning_min: Some(88.0),
            warning_max: Some(116.0),
            alarm_min: Some(78.0),
            alarm_max: Some(126.0),
        };
        let t0 = spec.points[0].0;
        spec.annotations = vec![AnnotationBand {
            start: t0 + Duration::hours(30),
            end: t0 + Duration::hours(46),
            text: "probe fouled".to_string(),
        }];
        spec.envelope = spec
            .points
            .iter()
            .map(|&(t, v)| (t, v - 5.0, v + 5.0))
            .collect();
        spec.gap_seconds = 10_800;
        let png = render_png(&spec).expect("render");
        std::fs::write(&path, png).expect("write");
        eprintln!("wrote {path}");
    }

    #[test]
    fn trim_number_drops_pointless_decimals() {
        assert_eq!(trim_number(12.0), "12");
        assert_eq!(trim_number(12.345), "12.35");
        assert_eq!(trim_number(f64::NAN), "–");
    }
}
