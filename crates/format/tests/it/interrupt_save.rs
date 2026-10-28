use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::Document;
use photocraft_format::{FormatError, SaveLimits, SaveOptions, load_from_bytes, save_to_bytes, save_to_bytes_interruptible};
use photocraft_geom::Size;
use photocraft_raster::{Interrupt, Rgba8Image};

fn document() -> Document {
    let mut doc = Document::new("controlled save", Size::new(6000, 4000), ColorMode::Rgb, SampleType::U8);
    for n in 0..16 {
        doc.metadata.psd_global_blocks.push((*b"8BIM", *b"test", Arc::new(vec![n; 256 << 10])));
    }
    doc
}

#[test]
fn save_is_lossless_and_worker_count_is_capped() {
    let doc = document();
    let options = SaveOptions::default();
    let expected = save_to_bytes(&doc, &options).unwrap();
    for workers in [1, 2] {
        let threads = Mutex::new(HashSet::new());
        let callback = || {
            threads.lock().unwrap().insert(std::thread::current().id());
            false
        };
        let limits = SaveLimits { max_workers: workers, ..Default::default() };
        let bytes = save_to_bytes_interruptible(&doc, &options, &limits, &Interrupt::cancel_only(&callback)).unwrap();
        assert_eq!(bytes, expected);
        assert_eq!(load_from_bytes(&bytes).unwrap(), doc);
        let count = threads.lock().unwrap().len();
        assert!(count <= if workers == 1 { 1 } else { workers + 1 }, "{count} threads for {workers} workers");
    }
}

#[test]
fn save_refuses_each_explicit_budget() {
    let doc = document();
    let options = SaveOptions { thumbnail: Some(Rgba8Image::new(2, 2)), composite: None };
    for limits in [
        SaveLimits { max_total_bytes: 1, ..Default::default() },
        SaveLimits { max_manifest_bytes: 1, ..Default::default() },
        SaveLimits { max_output_bytes: 1, ..Default::default() },
        SaveLimits { max_preview_pixels: 1, ..Default::default() },
        SaveLimits { max_workers: 0, ..Default::default() },
    ] {
        assert!(matches!(save_to_bytes_interruptible(&doc, &options, &limits, &Interrupt::NONE), Err(FormatError::LimitExceeded(_))));
    }
}

#[test]
fn already_cancelled_save_never_returns_an_archive() {
    assert!(matches!(
        save_to_bytes_interruptible(&document(), &SaveOptions::default(), &SaveLimits::default(), &Interrupt::cancel_only(&|| true)),
        Err(FormatError::Cancelled(_))
    ));
}

#[test]
fn in_flight_save_discards_partial_results_and_reports_latency() {
    let doc = document();
    let limits = SaveLimits { max_workers: 1, ..Default::default() };
    let calls = AtomicUsize::new(0);
    let count = || {
        calls.fetch_add(1, Ordering::Relaxed);
        false
    };
    save_to_bytes_interruptible(&doc, &SaveOptions::default(), &limits, &Interrupt::cancel_only(&count)).unwrap();
    let total = calls.load(Ordering::Relaxed);
    let mut worst = std::time::Duration::ZERO;
    for checkpoint in [0, 20, total / 2, total - 2] {
        calls.store(0, Ordering::Relaxed);
        let flipped = Mutex::new(None);
        let cancel = || {
            let n = calls.fetch_add(1, Ordering::Relaxed);
            if n == checkpoint {
                *flipped.lock().unwrap() = Some(std::time::Instant::now());
            }
            n > checkpoint
        };
        assert!(matches!(
            save_to_bytes_interruptible(&doc, &SaveOptions::default(), &limits, &Interrupt::cancel_only(&cancel)),
            Err(FormatError::Cancelled(_))
        ));
        worst = worst.max(flipped.lock().unwrap().unwrap().elapsed());
    }
    eprintln!("4 MiB save sampled worst cancellation-to-return after false checkpoint: {worst:?}");
}

#[test]
fn cached_saves_reapply_budgets_and_survive_cancellation() {
    let doc = document();
    let mut writer = photocraft_format::PcraftWriter::new();
    let opts = SaveOptions::default();
    let limits = SaveLimits { max_workers: 1, ..Default::default() };
    let (expected, _) = writer.save_zip_interruptible(&doc, &opts, &limits, &Interrupt::NONE).unwrap();
    let tiny = SaveLimits { max_total_bytes: 1, ..limits };
    assert!(matches!(writer.save_zip_interruptible(&doc, &opts, &tiny, &Interrupt::NONE), Err(FormatError::LimitExceeded(_))));
    let calls = AtomicUsize::new(0);
    let count = || {
        calls.fetch_add(1, Ordering::Relaxed);
        false
    };
    writer.save_zip_interruptible(&doc, &opts, &limits, &Interrupt::cancel_only(&count)).unwrap();
    let end = calls.swap(0, Ordering::Relaxed) - 2;
    let cancel = || calls.fetch_add(1, Ordering::Relaxed) >= end;
    assert!(matches!(writer.save_zip_interruptible(&doc, &opts, &limits, &Interrupt::cancel_only(&cancel)), Err(FormatError::Cancelled(_))));
    assert_eq!(writer.save_zip_interruptible(&doc, &opts, &limits, &Interrupt::NONE).unwrap().0, expected);
}
