//! Running a kernel on input nobody vetted (a server rendering an upload's smart filters): the
//! budgets a controlled kernel checks before it allocates, iterates or draws, and what it returns
//! instead of the desktop's quiet fallbacks.
//!
//! The special kernels (Puppet and Perspective Warp, Lens Correction, Adaptive Wide Angle, Camera
//! Raw) each run as one unit on the desktop, sized by caps chosen for an interactive editor
//! (Puppet's 2³⁰-pixel result, a warp's 2²⁰ destination tiles), and leave the pixels alone where
//! those caps refuse. Their `*_controlled` twins take a [`Budget`] and an [`Interrupt`]: the
//! interrupt is asked per row, tile, mesh insertion or solver iteration, so a cancel takes effect
//! within one of those; work, output and buffers past the budget are a [`Refused::Limit`] before
//! they happen. The desktop functions keep their behaviour and results: they are the controlled
//! ones with no interrupt and the old caps.

use std::fmt;

use photocraft_raster::{Cancelled, Interrupt};

/// What one controlled kernel run may produce and hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Most pixels any surface the kernel makes may span (its bounds, whatever it stores).
    pub max_pixels: u64,
    /// Most bytes of working buffers held at once, every worker of the pool counted.
    pub max_bytes: u64,
    /// Most solver and geometry steps, in units of one inner-loop step: mesh entries times
    /// iterations, triangle-pixel tests, lookups per pin, map evaluations.
    pub max_work: u64,
}

impl Budget {
    /// No limit at all (the desktop's own caps still apply where a kernel has them).
    pub const UNLIMITED: Budget = Budget { max_pixels: u64::MAX, max_bytes: u64::MAX, max_work: u64::MAX };

    /// `Err(Limit)` when a surface of `w × h` pixels is over [`Budget::max_pixels`].
    pub fn pixels(&self, what: &str, w: f64, h: f64) -> Result<(), Refused> {
        // `max` would turn a NaN side into 0.
        let area = w.max(0.0) * h.max(0.0);
        if w.is_nan() || h.is_nan() || !area.is_finite() || area > self.max_pixels as f64 {
            return Err(Refused::Limit(format!("{what} would cover {w:.0} × {h:.0} pixels; the limit is {}", self.max_pixels)));
        }
        Ok(())
    }

    /// `Err(Limit)` when `bytes` of buffers are over [`Budget::max_bytes`].
    pub fn bytes(&self, what: &str, bytes: u64) -> Result<(), Refused> {
        if bytes > self.max_bytes {
            return Err(Refused::Limit(format!("{what} needs an estimated {bytes} bytes; the limit is {}", self.max_bytes)));
        }
        Ok(())
    }

    /// `Err(Limit)` when `steps` of work are over [`Budget::max_work`].
    pub fn work(&self, what: &str, steps: u64) -> Result<(), Refused> {
        if steps > self.max_work {
            return Err(Refused::Limit(format!("{what} needs about {steps} steps; the limit is {}", self.max_work)));
        }
        Ok(())
    }
}

/// Why a controlled kernel made nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The interrupt was set; whatever was made so far is dropped.
    Cancelled,
    /// A [`Budget`] (or a structural cap of the controlled kernel) refused the work before it
    /// was done. The message says which and by how much.
    Limit(String),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Cancelled => f.write_str("cancelled"),
            Refused::Limit(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Refused {}

impl From<Cancelled> for Refused {
    fn from(_: Cancelled) -> Self {
        Refused::Cancelled
    }
}

/// `Err(Cancelled)` once `ctl` is set, for `?` between stages.
pub(crate) fn check(ctl: &Interrupt<'_>) -> Result<(), Refused> {
    if ctl.cancelled() { Err(Refused::Cancelled) } else { Ok(()) }
}

/// Bytes of `pixels` samples of `channels` f32 each.
pub(crate) fn f32_bytes(pixels: u64, channels: usize) -> u64 {
    pixels.saturating_mul(channels as u64).saturating_mul(4)
}

/// Threads a parallel kernel runs on: each may hold its own working window.
pub(crate) fn workers() -> u64 {
    crate::par::threads().max(1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_refuse_past_their_limit_and_name_what() {
        let b = Budget { max_pixels: 100, max_bytes: 1000, max_work: 10 };
        assert_eq!(b.pixels("x", 10.0, 10.0), Ok(()));
        assert!(matches!(b.pixels("the warp", 10.0, 11.0), Err(Refused::Limit(m)) if m.starts_with("the warp")));
        assert!(b.pixels("x", f64::INFINITY, 1.0).is_err());
        assert!(b.pixels("x", f64::NAN, 1.0).is_err());
        assert_eq!(b.bytes("x", 1000), Ok(()));
        assert!(b.bytes("x", 1001).is_err());
        assert_eq!(b.work("x", 10), Ok(()));
        assert!(b.work("x", 11).is_err());
        assert!(Budget::UNLIMITED.pixels("x", 1e9, 1e9).is_ok());
    }

    #[test]
    fn a_set_interrupt_is_cancelled() {
        let yes = || true;
        assert_eq!(check(&Interrupt::cancel_only(&yes)), Err(Refused::Cancelled));
        assert_eq!(check(&Interrupt::NONE), Ok(()));
        assert_eq!(Refused::from(Cancelled), Refused::Cancelled);
    }
}
