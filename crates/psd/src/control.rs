//! Caller budgets and cooperative cancellation for untrusted PSD input.

use crate::{PsdError, Result};

/// Limits for parsing and decoding. These bound decoded data, not all allocator overhead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadOptions {
    /// Maximum encoded file size.
    pub max_input_bytes: usize,
    /// Maximum pixels in the canvas or any layer/channel rectangle.
    pub max_pixels: u64,
    /// Maximum layer records.
    pub max_layers: usize,
    /// Maximum bytes in one decoded plane or merged image.
    pub max_decoded_bytes: u64,
    /// Maximum sum of declared layer channel and merged image bytes.
    pub max_total_decoded_bytes: u64,
    /// Maximum declared pattern and smart-filter cache bytes during document import.
    pub max_auxiliary_bytes: u64,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            max_input_bytes: usize::MAX,
            max_pixels: 300_000 * 300_000u64,
            max_layers: 32768,
            max_decoded_bytes: crate::compression::MAX_DECODED_BYTES,
            max_total_decoded_bytes: u64::MAX,
            max_auxiliary_bytes: u64::MAX,
        }
    }
}

/// A std-only hook: hosts can bridge their cancellation mechanism without a workspace dependency.
#[derive(Clone, Copy)]
pub struct ReadControl<'a> {
    /// Explicit allocation budgets.
    pub limits: LoadOptions,
    /// Called between records, rows and inflation chunks. Must remain true after cancellation.
    pub cancelled: &'a (dyn Fn() -> bool + Sync),
}

impl std::fmt::Debug for ReadControl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadControl").field("limits", &self.limits).finish_non_exhaustive()
    }
}

impl Default for ReadControl<'_> {
    fn default() -> Self {
        Self { limits: LoadOptions::default(), cancelled: &|| false }
    }
}

impl ReadControl<'_> {
    /// Return a typed cancellation error without partially decoded output.
    pub fn check(&self) -> Result<()> {
        if (self.cancelled)() { Err(PsdError::Cancelled) } else { Ok(()) }
    }

    /// Validate a decoded layout against pixel and per-decode limits before allocating.
    pub fn check_layout(&self, layout: &crate::PlaneLayout) -> Result<usize> {
        self.check()?;
        if (layout.width as u64).saturating_mul(layout.height as u64) > self.limits.max_pixels {
            return Err(PsdError::LimitExceeded("channel pixel budget"));
        }
        let n = layout.decoded_len()?;
        if n as u64 > self.limits.max_decoded_bytes {
            return Err(PsdError::LimitExceeded("decoded byte budget"));
        }
        Ok(n)
    }
}
