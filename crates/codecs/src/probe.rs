//! Header-only probe control; no workspace dependencies.

use crate::{CodecError, Limits};
use std::io::{Cursor, Read, Seek, SeekFrom};

/// Limits and cooperative cancellation for header-only inspection.
#[derive(Clone, Copy)]
pub struct ProbeControl<'a> {
    /// Dimension limits and budgets passed to the underlying header readers.
    pub limits: Limits,
    /// Maximum complete encoded input size, checked before parsing.
    pub max_input_bytes: usize,
    /// Must remain true once cancellation is requested.
    pub cancelled: &'a (dyn Fn() -> bool + Sync),
}

impl Default for ProbeControl<'_> {
    fn default() -> Self {
        Self { limits: Limits::default(), max_input_bytes: usize::MAX, cancelled: &|| false }
    }
}

impl std::fmt::Debug for ProbeControl<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeControl").field("limits", &self.limits).field("max_input_bytes", &self.max_input_bytes).finish_non_exhaustive()
    }
}

impl ProbeControl<'_> {
    /// Return a typed error once cancellation is requested.
    pub fn check(&self) -> Result<(), CodecError> {
        if (self.cancelled)() { Err(CodecError::Cancelled) } else { Ok(()) }
    }
    pub(crate) fn input(&self, bytes: &[u8]) -> Result<(), CodecError> {
        self.check()?;
        if bytes.len() > self.max_input_bytes {
            return Err(CodecError::LimitExceeded("encoded input byte budget".into()));
        }
        Ok(())
    }
}

pub(crate) struct ProbeReader<'a> {
    cursor: Cursor<&'a [u8]>,
    ctl: ProbeControl<'a>,
}

impl<'a> ProbeReader<'a> {
    pub(crate) fn new(bytes: &'a [u8], ctl: ProbeControl<'a>) -> Self {
        Self { cursor: Cursor::new(bytes), ctl }
    }
}

impl Read for ProbeReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        self.ctl.check().map_err(std::io::Error::other)?;
        let len = out.len().min(64 << 10);
        self.cursor.read(out.get_mut(..len).ok_or_else(|| std::io::Error::other("invalid read buffer"))?)
    }
}
impl Seek for ProbeReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.ctl.check().map_err(std::io::Error::other)?;
        self.cursor.seek(pos)
    }
}
