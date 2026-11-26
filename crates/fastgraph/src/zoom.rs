//! Zooming a graph axis: the stretch of an axis in view, and how a wheel
//! notch over the axis moves it. The frequency axis is zoomed in decades
//! (log10 Hz); a level axis in its own units (dB, degrees, or the decades
//! of a percentage). Free of any UI type so it can be unit-tested.
use crate::plot::{F_MAX, F_MIN};

/// The span changes by this factor per wheel notch.
pub const STEP: f64 = 1.15;
/// The pixels of a smooth (touchpad) scroll that make one notch.
pub const PIXELS_PER_LINE: f32 = 60.0;
/// A level axis zooms out to this many times its default span.
pub const LEVEL_OUT: f64 = 2.0;

/// A stretch of an axis, `low` below `high`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub low: f64,
    pub high: f64,
}

impl Range {
    pub fn new(low: f64, high: f64) -> Self {
        Self { low, high }
    }

    pub fn span(self) -> f64 {
        self.high - self.low
    }

    pub fn centre(self) -> f64 {
        (self.low + self.high) / 2.0
    }

    /// `self` moved as a whole to lie inside `within`; one wider than
    /// `within` is centred on it.
    pub fn kept_in(self, within: Range) -> Range {
        if self.span() >= within.span() {
            let half = self.span() / 2.0;
            let centre = within.centre();
            return Range::new(centre - half, centre + half);
        }
        // Moved onto an edge, it ends exactly there.
        if self.low < within.low {
            Range::new(within.low, within.low + self.span())
        } else if self.high > within.high {
            Range::new(within.high - self.span(), within.high)
        } else {
            self
        }
    }

    /// Whether the span is finite and positive: a range an axis can show.
    fn usable(self) -> bool {
        self.span().is_finite() && self.span() > 0.0
    }

    /// Whether `self` is `other` to within a billionth of its span.
    fn is(self, other: Range) -> bool {
        let eps = other.span().abs() * 1e-9;
        (self.low - other.low).abs() <= eps && (self.high - other.high).abs() <= eps
    }
}

/// How far an axis zooms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// The automatic fit, shown when the axis is not zoomed.
    pub default: Range,
    /// All there is to see: the axis zoomed fully out.
    pub full: Range,
    /// The least span zooming in shows.
    pub min_span: f64,
}

impl Limits {
    /// The frequency axis, in decades: 20 Hz–20 kHz by default and at
    /// most, and at least a third of an octave.
    pub fn frequency() -> Self {
        let full = Range::new(F_MIN.log10(), F_MAX.log10());
        Self {
            default: full,
            full,
            min_span: 2f64.log10() / 3.0,
        }
    }

    /// A level axis fitted to `default`: it zooms out to [`LEVEL_OUT`]
    /// times that span about its centre, and in to `min_span`.
    pub fn level(default: Range, min_span: f64) -> Self {
        let half = default.span() * LEVEL_OUT / 2.0;
        let centre = default.centre();
        Self {
            default,
            full: Range::new(centre - half, centre + half),
            min_span,
        }
    }
}

/// The span factor of a scroll of `lines` notches: below 1 (zooming in)
/// for a scroll up, a positive `lines`.
pub fn factor(lines: f64) -> f64 {
    STEP.powf(-lines)
}

/// `range` (the axis as shown) with its span scaled by `factor` about
/// `at`, which keeps its place on the axis, within `limits`: the span stays
/// between the least and the full one, and the range inside the full one.
/// `None` when that is the default, so an axis zoomed back out is no longer
/// zoomed, and when `range` or the result has no finite positive span (its
/// ends too large to tell apart).
pub fn zoom(range: Range, at: f64, factor: f64, limits: &Limits) -> Option<Range> {
    if !range.usable() {
        return None;
    }
    let at = at.clamp(range.low, range.high);
    let span = (range.span() * factor)
        .max(limits.min_span)
        .min(limits.full.span());
    let k = span / range.span();
    let low = at - (at - range.low) * k;
    let zoomed = Range::new(low, low + span).kept_in(limits.full);
    (zoomed.usable() && !zoomed.is(limits.default)).then_some(zoomed)
}

/// A view's zoom: each axis's range, `None` at its default fit.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ViewZoom {
    /// In decades (log10 Hz).
    pub freq: Option<Range>,
    pub level: Option<Range>,
}

impl ViewZoom {
    pub fn is_zoomed(&self) -> bool {
        self.freq.is_some() || self.level.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    // The value under the pointer keeps its place: its share of the way
    // across the axis is the same before and after.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn zooming_keeps_the_pointer_value_in_place() {
        let limits = Limits::frequency();
        let full = limits.default;
        let at = 1000f64.log10();
        let share = |r: Range| (at - r.low) / r.span();
        let zoomed = zoom(full, at, factor(1.0), &limits).unwrap();
        assert!(close(zoomed.span(), full.span() / STEP));
        assert!(close(share(zoomed), share(full)));
        let again = zoom(zoomed, at, factor(2.5), &limits).unwrap();
        assert!(close(share(again), share(full)));
        // A level axis too, off centre.
        let limits = Limits::level(Range::new(40.0, 90.0), 5.0);
        let zoomed = zoom(limits.default, 80.0, factor(3.0), &limits).unwrap();
        assert!(close((80.0 - zoomed.low) / zoomed.span(), 0.8));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn frequency_zooms_out_to_the_full_decades_and_in_to_a_third_octave() {
        let limits = Limits::frequency();
        // Zooming out at the default does nothing.
        assert_eq!(zoom(limits.default, 2.0, factor(-3.0), &limits), None);
        // Far in, near the top: a third of an octave, kept inside 20 kHz.
        let at = 19_000f64.log10();
        let deep = zoom(limits.default, at, factor(100.0), &limits).unwrap();
        assert!(close(deep.span(), limits.min_span));
        assert!(close(10f64.powf(deep.span()), 2f64.powf(1.0 / 3.0)));
        assert!(deep.high <= F_MAX.log10() && deep.low >= F_MIN.log10());
        assert!(deep.low <= at && at <= deep.high);
        // Near the bottom, kept above 20 Hz.
        let low = zoom(limits.default, F_MIN.log10(), factor(100.0), &limits).unwrap();
        assert!(close(low.low, F_MIN.log10()));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn level_zooms_out_to_twice_the_fit_and_in_to_its_least_span() {
        for (default, least) in [
            (Range::new(40.0, 90.0), 5.0),     // dB
            (Range::new(-180.0, 180.0), 10.0), // degrees
            (Range::new(-2.0, 2.0), 1.0),      // percent decades
            (Range::new(-160.0, 15.0), 5.0),   // distortion dB
        ] {
            let limits = Limits::level(default, least);
            let out = zoom(default, default.high, factor(-50.0), &limits).unwrap();
            assert!(close(out.span(), 2.0 * default.span()), "{default:?}");
            // Centred on the fit, whatever the pointer.
            assert!(close(out.centre(), default.centre()), "{default:?}");
            let deep = zoom(default, default.low, factor(50.0), &limits).unwrap();
            assert!(close(deep.span(), least), "{default:?}");
            assert!(close(deep.low, default.low), "{default:?}");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn zooming_back_out_returns_exactly_to_the_default() {
        // Frequency: the default is the full range, so any zoom out reaches it.
        let limits = Limits::frequency();
        let mut range = limits.default;
        for (at, lines) in [(2.1, 2.0), (3.9, 1.0), (2.5, 3.0)] {
            range = zoom(range, at, factor(lines), &limits).unwrap();
        }
        assert_eq!(zoom(range, 3.0, factor(-30.0), &limits), None);
        // Level: back out at the same place by as many notches.
        let limits = Limits::level(Range::new(-35.0, 15.0), 5.0);
        let mut range = limits.default;
        for _ in 0..4 {
            range = zoom(range, 3.7, factor(1.0), &limits).unwrap();
        }
        let mut last = None;
        for _ in 0..4 {
            last = zoom(range, 3.7, factor(-1.0), &limits);
            range = last.unwrap_or(limits.default);
        }
        assert_eq!(last, None);
        // A touchpad's fractions add up the same way.
        let mut range = limits.default;
        for _ in 0..10 {
            range = zoom(range, -20.0, factor(0.3), &limits).unwrap();
        }
        let mut back = Some(range);
        for _ in 0..10 {
            back = zoom(back.unwrap_or(limits.default), -20.0, factor(-0.3), &limits);
        }
        assert_eq!(back, None);
    }

    // A range with no finite positive span, given or computed, is declined:
    // it would otherwise be saved and drawn.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn a_zoom_without_a_usable_span_is_declined() {
        let limits = Limits::level(Range::new(40.0, 90.0), 5.0);
        for range in [
            Range::new(1e300, 1e300),
            Range::new(-f64::MAX, f64::MAX),
            Range::new(f64::NAN, 1.0),
        ] {
            assert_eq!(zoom(range, 1.0, factor(1.0), &limits), None, "{range:?}");
        }
        // Its fit itself has no span: the endpoints round together.
        let flat = Limits::level(Range::new(1e300, 1e300), 5.0);
        assert_eq!(zoom(flat.default, 1e300, factor(1.0), &flat), None);
        // A usable range whose zoomed endpoints round together at its level.
        let far = Limits::level(Range::new(1e20, 1e20 + 1e6), 5.0);
        assert_eq!(zoom(far.default, 1e20, factor(50.0), &far), None);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_range_is_kept_inside_the_full_one() {
        let full = Range::new(0.0, 10.0);
        assert_eq!(Range::new(-2.0, 3.0).kept_in(full), Range::new(0.0, 5.0));
        assert_eq!(Range::new(8.0, 12.0).kept_in(full), Range::new(6.0, 10.0));
        assert_eq!(Range::new(2.0, 4.0).kept_in(full), Range::new(2.0, 4.0));
        assert_eq!(
            Range::new(-5.0, 25.0).kept_in(full),
            Range::new(-10.0, 20.0)
        );
    }
}
