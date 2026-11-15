//! Per-document native export cache; file writes and save identity stay with the caller.

use std::sync::{Arc, Mutex};

use photocraft_doc::Document;
use photocraft_format::{PcraftWriter, SaveOptions};

/// Shared native export cache. Clones reuse tile hashes and up to 64 MiB of compressed payloads;
/// dropping the last clone releases it, including when a document closes after a save worker.
#[derive(Clone)]
pub struct PcraftExportCache(Arc<Mutex<PcraftWriter>>);

impl Default for PcraftExportCache {
    fn default() -> Self {
        let mut writer = PcraftWriter::new();
        writer.set_compressed_cache_budget(64 << 20);
        Self(Arc::new(Mutex::new(writer)))
    }
}

impl std::fmt::Debug for PcraftExportCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PcraftExportCache")
    }
}

impl PartialEq for PcraftExportCache {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl PcraftExportCache {
    pub(crate) fn encode(&self, doc: &Document, opts: &SaveOptions) -> photocraft_format::Result<Vec<u8>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).save_zip(doc, opts).map(|(bytes, _)| bytes)
    }

    /// Prepares a recovery snapshot of `doc` as separate parts ([`photocraft_format::Parts`]):
    /// hashes only what this cache does not know yet, so it shares the tile hashes with File ›
    /// Save. Stops with `Cancelled` when `ctl` trips and resumes on the next call (a caller gives
    /// it a time budget per frame); the caller compresses the parts one by one.
    pub fn prepare_parts(&self, doc: &Document, ctl: &photocraft_raster::Interrupt) -> photocraft_format::Result<photocraft_format::Parts> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).prepare_parts(doc, &photocraft_format::SaveLimits::default(), ctl)
    }

    /// [`Self::prepare_parts`] until `over` says the caller's time is up. Also answers whether
    /// the call got further (hashed something new), so a caller can tell a document that needs
    /// more steps from one object too large for its budget.
    pub fn prepare_parts_until(&self, doc: &Document, over: &(dyn Fn() -> bool + Sync)) -> (photocraft_format::Result<photocraft_format::Parts>, bool) {
        let mut writer = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = writer.known_hashes();
        let result = writer.prepare_parts(doc, &photocraft_format::SaveLimits::default(), &photocraft_raster::Interrupt::cancel_only(over));
        (result, writer.known_hashes() > before)
    }
}

/// A `.pcraft` of a recovery copy's parts ([`photocraft_format::bundle_from_parts`]), without
/// cancellation: made once, while the page opening it shows that it is loading.
pub fn bundle_from_parts(manifest: &[u8], objects: &[(String, Vec<u8>)]) -> photocraft_format::Result<Vec<u8>> {
    photocraft_format::bundle_from_parts(manifest, objects, &photocraft_raster::Interrupt::NONE)
}

/// Part `i` of `parts`, compressed as a bundle stores it ([`photocraft_format::Parts::compress`]).
pub fn compress_part(parts: &photocraft_format::Parts, i: usize) -> photocraft_format::Result<Vec<u8>> {
    parts.compress(i, &photocraft_raster::Interrupt::NONE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExportOptions, export};
    use photocraft_color::{ColorMode, SampleType};
    use photocraft_doc::{Layer, LayerContent};
    use photocraft_geom::Size;
    use photocraft_raster::Surface;

    #[test]
    fn cached_exports_reuse_tiles_and_preserve_edits_and_save_as() {
        for depth in SampleType::ALL {
            let mut doc = Document::new("first.pcraft", Size::new(2, 1), ColorMode::Rgb, depth);
            let mut surface = Surface::new(doc.pixel_format());
            surface.write_pixel(0, 0, &[0.2, 0.4, 0.6, 1.0]);
            doc.layers.push(Layer::new("photo", LayerContent::Raster(surface)));
            let cache = PcraftExportCache::default();
            let options = ExportOptions { pcraft_cache: Some(cache.clone()), ..Default::default() };
            let first = export(&doc, "first.pcraft", &options).unwrap();
            assert_eq!(photocraft_format::load_from_bytes(&first.bytes).unwrap(), doc);
            let (_, stats) = cache.0.lock().unwrap().save_zip(&doc, &SaveOptions::default()).unwrap();
            assert_eq!(stats.tiles_reused, stats.tiles_total);
            assert_eq!(stats.tiles_written, 0);
            doc.layers[0].surface_mut().unwrap().try_write_region(photocraft_geom::Rect::new(0, 0, 1, 1), &[0.8, 0.3, 0.1, 1.0]).unwrap();
            let next = export(&doc, "second.pcraft", &options).unwrap();
            let mut expected = doc.clone();
            expected.name = "second.pcraft".into();
            assert_eq!(photocraft_format::load_from_bytes(&next.bytes).unwrap(), expected);
            assert_eq!(next.bytes, export(&doc, "second.pcraft", &ExportOptions::default()).unwrap().bytes);
            // Recovery parts share the cache's hashes and make a bundle of the same document.
            let parts = cache.prepare_parts(&doc, &photocraft_raster::Interrupt::NONE).unwrap();
            let objects: Vec<_> =
                (0..parts.len()).map(|i| (parts.path(i).unwrap().to_owned(), parts.compress(i, &photocraft_raster::Interrupt::NONE).unwrap())).collect();
            let bundle = photocraft_format::bundle_from_parts(parts.manifest(), &objects, &photocraft_raster::Interrupt::NONE).unwrap();
            assert_eq!(photocraft_format::load_from_bytes(&bundle).unwrap(), doc);
            let weak = Arc::downgrade(&cache.0);
            drop(options);
            drop(cache);
            assert!(weak.upgrade().is_none());
        }
    }
}
