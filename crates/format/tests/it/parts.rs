use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::{Document, Layer, LayerContent};
use photocraft_format::{FormatError, PcraftWriter, SaveLimits, bundle_from_parts, load_from_bytes};
use photocraft_geom::{Rect, Size};
use photocraft_raster::{Interrupt, Surface};

/// Fills `rect` of `surface` with one colour (RGBA).
fn fill(surface: &mut Surface, rect: Rect, rgba: [f32; 4]) {
    let n = (rect.width() * rect.height()) as usize;
    surface.try_write_region(rect, &rgba.repeat(n)).unwrap();
}

/// Two raster layers over a 700×300 canvas: 3×2 tiles each, every tile its own colour.
fn document() -> Document {
    let mut doc = Document::new("parts", Size::new(700, 300), ColorMode::Rgb, SampleType::U8);
    for (n, name) in ["below", "above"].into_iter().enumerate() {
        let mut surface = Surface::new(doc.pixel_format());
        for ty in 0..2 {
            for tx in 0..3 {
                let shade = (n * 6 + ty * 3 + tx) as f32 / 16.0;
                let (x, y) = (tx * 256, ty * 256);
                let rect = Rect::from_xywh(x as i32, y as i32, (700 - x).min(256) as u32, (300 - y).min(256) as u32);
                fill(&mut surface, rect, [shade, 0.5, 1.0 - shade, 1.0]);
            }
        }
        doc.layers.push(Layer::new(name, LayerContent::Raster(surface)));
    }
    doc
}

/// Every part, compressed.
fn compress_all(parts: &photocraft_format::Parts) -> Vec<(String, Vec<u8>)> {
    (0..parts.len()).map(|i| (parts.path(i).unwrap().to_owned(), parts.compress(i, &Interrupt::NONE).unwrap())).collect()
}

#[test]
fn parts_make_a_bundle_that_opens_as_the_document() {
    let doc = document();
    let parts = PcraftWriter::new().prepare_parts(&doc, &SaveLimits::default(), &Interrupt::NONE).unwrap();
    assert_eq!(parts.len(), 12, "two layers of 3×2 tiles, all different");
    let objects = compress_all(&parts);
    let bundle = bundle_from_parts(parts.manifest(), &objects, &Interrupt::NONE).unwrap();
    assert_eq!(load_from_bytes(&bundle).unwrap(), doc);
    assert_eq!(photocraft_format::read_thumbnail(&bundle).unwrap(), None, "no previews");
}

#[test]
fn a_later_snapshot_names_only_the_changed_tiles_as_new() {
    let mut doc = document();
    let mut writer = PcraftWriter::new();
    let first = writer.prepare_parts(&doc, &SaveLimits::default(), &Interrupt::NONE).unwrap();
    let before: HashSet<String> = first.paths().map(str::to_owned).collect();
    // One pixel in one tile of one layer.
    fill(doc.layers[1].surface_mut().unwrap(), Rect::from_xywh(10, 10, 1, 1), [0.9, 0.1, 0.1, 1.0]);
    let second = writer.prepare_parts(&doc, &SaveLimits::default(), &Interrupt::NONE).unwrap();
    let new: Vec<&str> = second.paths().filter(|p| !before.contains(*p)).collect();
    assert_eq!(new.len(), 1, "only the edited tile is new: {new:?}");
    // The host keeps the earlier parts: the new one completes the bundle.
    let mut objects = compress_all(&first);
    objects.retain(|(path, _)| second.paths().any(|p| p == path));
    let i = (0..second.len()).find(|&i| second.path(i) == Some(new[0])).unwrap();
    objects.push((new[0].to_owned(), second.compress(i, &Interrupt::NONE).unwrap()));
    let bundle = bundle_from_parts(second.manifest(), &objects, &Interrupt::NONE).unwrap();
    assert_eq!(load_from_bytes(&bundle).unwrap(), doc);
}

/// How many times one `prepare_parts` call checks its interrupt.
fn checks(writer: &mut PcraftWriter, doc: &Document) -> usize {
    let count = AtomicUsize::new(0);
    let counting = || {
        count.fetch_add(1, Ordering::Relaxed);
        false
    };
    writer.prepare_parts(doc, &SaveLimits::default(), &Interrupt::cancel_only(&counting)).unwrap();
    count.into_inner()
}

#[test]
fn an_interrupted_prepare_keeps_its_hashes_and_the_next_call_resumes() {
    let doc = document();
    let whole = PcraftWriter::new().prepare_parts(&doc, &SaveLimits::default(), &Interrupt::NONE).unwrap();
    // What a cold prepare checks (hashing every tile) and a warm one (every hash known).
    let mut probe = PcraftWriter::new();
    let (cold, warm) = (checks(&mut probe, &doc), checks(&mut probe, &doc));
    assert!(warm < cold, "known hashes are not hashed again: {warm} vs {cold}");
    // A budget that fits the warm walk and a little hashing, never a whole cold prepare: every call
    // stops early until the hashes are known, and each one gets further.
    let budget = warm + (cold - warm) / 4;
    let mut writer = PcraftWriter::new();
    let mut calls = 0;
    let parts = loop {
        calls += 1;
        assert!(calls < 100, "an interrupted prepare must make progress");
        let used = AtomicUsize::new(0);
        let deadline = || used.fetch_add(1, Ordering::Relaxed) >= budget;
        match writer.prepare_parts(&doc, &SaveLimits::default(), &Interrupt::cancel_only(&deadline)) {
            Ok(parts) => break parts,
            Err(FormatError::Cancelled(_)) => continue,
            Err(e) => panic!("{e}"),
        }
    };
    assert!(calls > 1, "the budget interrupted at least once");
    assert_eq!(parts.manifest(), whole.manifest());
    assert_eq!(parts.paths().collect::<Vec<_>>(), whole.paths().collect::<Vec<_>>());
}

#[test]
fn a_bundle_of_parts_refuses_strange_paths_and_a_damaged_part_does_not_open() {
    let doc = document();
    let parts = PcraftWriter::new().prepare_parts(&doc, &SaveLimits::default(), &Interrupt::NONE).unwrap();
    let mut objects = compress_all(&parts);
    for bad in ["manifest.json", "tiles/../x.zst", "tiles/abc.zst", "other/0000000000000000000000000000000000000000000000000000000000000000.zst"] {
        let mut odd = objects.clone();
        odd.push((bad.to_owned(), vec![1, 2, 3]));
        assert!(bundle_from_parts(parts.manifest(), &odd, &Interrupt::NONE).is_err(), "{bad}");
    }
    let mut twice = objects.clone();
    twice.push(objects[0].clone());
    assert!(bundle_from_parts(parts.manifest(), &twice, &Interrupt::NONE).is_err(), "a path twice");
    // A part whose bytes are another part's: the content hash check refuses the load.
    let other = objects[1].1.clone();
    objects[0].1 = other;
    let bundle = bundle_from_parts(parts.manifest(), &objects, &Interrupt::NONE).unwrap();
    assert!(load_from_bytes(&bundle).is_err());
}

#[test]
fn a_large_blob_goes_into_its_part_uncompressed_and_still_opens() {
    let mut doc = document();
    // A compressible blob over the threshold: stored raw, so its part is at least its size.
    let blob = std::sync::Arc::new(vec![7u8; photocraft_format::PARTS_RAW_BYTES + 1]);
    doc.metadata.psd_global_blocks.push((*b"8BIM", *b"test", blob.clone()));
    let parts = PcraftWriter::new().prepare_parts(&doc, &SaveLimits::default(), &Interrupt::NONE).unwrap();
    let objects = compress_all(&parts);
    let raw_bytes: usize = objects.iter().filter(|(path, _)| path.starts_with("blobs/")).map(|(_, raw)| raw.len()).sum();
    assert_eq!(raw_bytes, blob.len(), "raw fragments keep the original bytes");
    let bundle = bundle_from_parts(parts.manifest(), &objects, &Interrupt::NONE).unwrap();
    assert_eq!(load_from_bytes(&bundle).unwrap(), doc);
}

#[test]
fn a_single_large_blob_resumes_hashing_without_extending_the_budget() {
    let mut doc = Document::new("blob", Size::new(1, 1), ColorMode::Rgb, SampleType::U8);
    doc.metadata.exif = Some(std::sync::Arc::new(vec![23; 128 << 20]));
    let mut probe = PcraftWriter::new();
    let cold = checks(&mut probe, &doc);
    let warm = checks(&mut probe, &doc);
    assert!(cold > warm);
    let budget = warm + 32;
    assert!(budget < 2048, "warm {warm}, cold {cold}, budget {budget}");
    let mut writer = PcraftWriter::new();
    for _ in 0..32 {
        let used = AtomicUsize::new(0);
        let over = || used.fetch_add(1, Ordering::Relaxed) >= budget;
        match writer.prepare_parts(&doc, &SaveLimits::default(), &Interrupt::cancel_only(&over)) {
            Ok(_) => return,
            Err(FormatError::Cancelled(_)) => {}
            Err(e) => panic!("{e}"),
        }
    }
    panic!("a single blob must make progress at a fixed per-step budget");
}

#[test]
fn a_large_blob_has_bounded_recovery_transfers_and_round_trips() {
    let mut doc = Document::new("blob", Size::new(1, 1), ColorMode::Rgb, SampleType::U8);
    doc.metadata.exif = Some(std::sync::Arc::new(vec![23; 8 << 20]));
    let parts = PcraftWriter::new().prepare_parts(&doc, &SaveLimits::default(), &Interrupt::NONE).unwrap();
    let objects = compress_all(&parts);
    assert!(objects.len() > 1);
    assert!(objects.iter().all(|(_, bytes)| bytes.len() <= 256 << 10));
    let bundle = bundle_from_parts(parts.manifest(), &objects, &Interrupt::NONE).unwrap();
    assert_eq!(load_from_bytes(&bundle).unwrap(), doc);
}

#[test]
fn raw_fragments_reject_missing_corrupt_short_and_ambiguous_objects() {
    let mut doc = Document::new("blob", Size::new(1, 1), ColorMode::Rgb, SampleType::U8);
    doc.metadata.exif = Some(std::sync::Arc::new(vec![23; 2 << 20]));
    let mut writer = PcraftWriter::new();
    let before = writer.save_zip(&doc, &Default::default()).unwrap().0;
    let parts = writer.prepare_parts(&doc, &SaveLimits::default(), &Interrupt::NONE).unwrap();
    let objects = compress_all(&parts);
    assert_eq!(writer.save_zip(&doc, &Default::default()).unwrap().0, before, "recovery does not change save bytes");
    let mut missing = objects.clone();
    missing.remove(1);
    assert!(bundle_from_parts(parts.manifest(), &missing, &Interrupt::NONE).is_err());
    let mut short = objects.clone();
    short[0].1.pop();
    assert!(bundle_from_parts(parts.manifest(), &short, &Interrupt::NONE).is_err());
    let mut oversized = objects.clone();
    oversized[0].1.push(1);
    assert!(bundle_from_parts(parts.manifest(), &oversized, &Interrupt::NONE).is_err());
    let mut mixed = objects.clone();
    mixed.push((objects[0].0.split_once(".part/").unwrap().0.to_owned(), vec![1]));
    assert!(bundle_from_parts(parts.manifest(), &mixed, &Interrupt::NONE).is_err());
    let mut corrupt = objects.clone();
    corrupt[0].1[0] ^= 1;
    let bundle = bundle_from_parts(parts.manifest(), &corrupt, &Interrupt::NONE).unwrap();
    assert!(load_from_bytes(&bundle).is_err(), "whole-object hash still validates every fragment");
    let mut tail = objects.clone();
    tail.pop();
    let bundle = bundle_from_parts(parts.manifest(), &tail, &Interrupt::NONE).unwrap();
    assert!(load_from_bytes(&bundle).is_err(), "missing final fragment cannot restore a partial object");
    let mut reversed = objects;
    reversed.reverse();
    let bundle = bundle_from_parts(parts.manifest(), &reversed, &Interrupt::NONE).unwrap();
    assert_eq!(load_from_bytes(&bundle).unwrap(), doc);
}

#[test]
fn oversized_float_tiles_keep_the_fast_uncompressed_recovery_path() {
    let mut doc = Document::new("float tile", Size::new(256, 256), ColorMode::Cmyk, SampleType::F32);
    let mut surface = Surface::new(doc.pixel_format());
    surface.try_write_region(Rect::from_xywh(0, 0, 256, 256), &[0.1, 0.2, 0.3, 0.4, 1.0].repeat(256 * 256)).unwrap();
    doc.layers.push(Layer::new("pixels", LayerContent::Raster(surface)));
    let parts = PcraftWriter::new().prepare_parts(&doc, &SaveLimits::default(), &Interrupt::NONE).unwrap();
    let objects = compress_all(&parts);
    assert_eq!(objects.len(), 1);
    assert!(objects[0].1.len() > 256 * 256 * 5 * 4, "oversized tiles must retain raw encoding, not run the full compressor");
    let bundle = bundle_from_parts(parts.manifest(), &objects, &Interrupt::NONE).unwrap();
    assert_eq!(load_from_bytes(&bundle).unwrap(), doc);
}
