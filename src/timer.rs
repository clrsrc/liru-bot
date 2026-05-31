//! A timer for use in liru-bot.
//!
//! Mirrors `lib/timer.py`. We keep the Python free-function helpers
//! (`msec`, `seconds`, `minutes`, `to_msec`, …) as a thin layer over
//! `std::time::Duration` so call sites stay readable.

use std::time::{Duration, Instant};

/// `Duration::from_secs_f64` hardened against the inputs it panics on
/// (negative, NaN, infinite, or out of `Duration` range). Clock values come
/// straight from the Lichess NDJSON stream, so a malformed frame must
/// saturate to a sane duration instead of crashing the game task.
#[inline]
fn secs_f64_saturating(secs: f64) -> Duration {
    if !secs.is_finite() || secs <= 0.0 {
        return Duration::ZERO;
    }
    Duration::try_from_secs_f64(secs).unwrap_or(Duration::MAX)
}

/// `timedelta(milliseconds=time_in_msec)`.
#[inline]
pub fn msec(time_in_msec: f64) -> Duration {
    secs_f64_saturating(time_in_msec / 1000.0)
}

/// Length of `duration` in (possibly fractional) milliseconds.
#[inline]
pub fn to_msec(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Whole-millisecond string representation (matches Python's `str(round(...))`).
#[inline]
pub fn msec_str(duration: Duration) -> String {
    format!("{}", to_msec(duration).round() as i64)
}

#[inline]
pub fn seconds(time_in_sec: f64) -> Duration {
    secs_f64_saturating(time_in_sec)
}

#[inline]
pub fn to_seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

#[inline]
pub fn sec_str(duration: Duration) -> String {
    format!("{}", to_seconds(duration).round() as i64)
}

#[inline]
pub fn minutes(time_in_minutes: f64) -> Duration {
    seconds(time_in_minutes * 60.0)
}

#[inline]
pub fn hours(time_in_hours: f64) -> Duration {
    seconds(time_in_hours * 3600.0)
}

#[inline]
pub fn days(time_in_days: f64) -> Duration {
    seconds(time_in_days * 86_400.0)
}

#[inline]
pub fn years(time_in_years: f64) -> Duration {
    days(365.0 * time_in_years)
}

pub const ZERO: Duration = Duration::ZERO;

/// Countdown timer / stopwatch.
///
/// If `duration` is greater than zero, `is_expired()` indicates when the
/// initial duration has passed. Regardless of the initial duration the
/// timer can be used as a stopwatch via `time_since_reset()`.
#[derive(Debug, Clone, Copy)]
pub struct Timer {
    duration: Duration,
    starting_time: Instant,
}

impl Timer {
    pub fn new(duration: Duration) -> Self {
        Self { duration, starting_time: Instant::now() }
    }

    pub fn zero() -> Self {
        Self::new(ZERO)
    }

    pub fn is_expired(&self) -> bool {
        self.time_since_reset() >= self.duration
    }

    pub fn reset(&mut self) {
        self.starting_time = Instant::now();
    }

    pub fn time_since_reset(&self) -> Duration {
        self.starting_time.elapsed()
    }

    pub fn time_until_expiration(&self) -> Duration {
        self.duration.checked_sub(self.time_since_reset()).unwrap_or(ZERO)
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::zero()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_helpers_round_trip() {
        assert_eq!(to_msec(msec(1500.0)), 1500.0);
        assert_eq!(to_seconds(seconds(2.5)), 2.5);
        assert_eq!(minutes(2.0), seconds(120.0));
        assert_eq!(hours(1.0), seconds(3600.0));
        assert_eq!(days(1.0), seconds(86_400.0));
        assert_eq!(years(1.0), days(365.0));
    }

    #[test]
    fn timer_starts_unexpired_and_can_reset() {
        let mut t = Timer::new(seconds(10.0));
        assert!(!t.is_expired());
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(t.time_since_reset().as_millis() >= 5);
        t.reset();
        assert!(t.time_since_reset().as_millis() < 5);
    }

    #[test]
    fn zero_timer_is_immediately_expired() {
        let t = Timer::zero();
        assert!(t.is_expired());
        assert_eq!(t.time_until_expiration(), ZERO);
    }

    #[test]
    fn msec_str_rounds_half_to_even_or_nearest() {
        assert_eq!(msec_str(msec(1500.0)), "1500");
        assert_eq!(sec_str(seconds(0.6)), "1");
    }

    // Ported from `test_bot/test_timer.py::test_time_conversion` — verifies
    // the cross-unit math: 1 minute = 60 s, 1 hour = 3600 s, etc.
    #[test]
    fn cross_unit_conversions_match_python() {
        assert_eq!(to_msec(seconds(1.0)), 1000.0);
        assert_eq!(to_seconds(minutes(1.0)), 60.0);
        assert_eq!(to_seconds(hours(1.0)), 60.0 * 60.0);
        assert_eq!(to_seconds(days(1.0)), 24.0 * 60.0 * 60.0);
        assert_eq!(to_seconds(years(1.0)), 365.0 * 24.0 * 60.0 * 60.0);
    }

    // Ported from `test_bot/test_timer.py::test_time` — verifies
    // saturation: once the duration has been "more than used up",
    // `time_until_expiration` clamps to zero rather than going negative.
    #[test]
    fn time_until_expiration_saturates_at_zero() {
        let t = Timer::new(seconds(0.0));
        assert_eq!(t.time_until_expiration(), ZERO);
    }
}
