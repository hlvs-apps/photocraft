use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::Document;
use photocraft_format::{FormatError, LoadOptions, SaveOptions, load_from_bytes_interruptible, save_to_bytes};
use photocraft_geom::Size;
use photocraft_raster::Interrupt;

fn bundle() -> (Document, Vec<u8>) {
    let mut doc = Document::new("large blob", Size::new(6000, 4000), ColorMode::Rgb, SampleType::U8);
    doc.icc_profile = Some(Arc::new(vec![42; 8 << 20]));
    let bytes = save_to_bytes(&doc, &SaveOptions::default()).unwrap();
    (doc, bytes)
}

#[test]
fn already_cancelled_load_never_starts() {
    let (_, bytes) = bundle();
    let ctl = Interrupt::cancel_only(&|| true);
    assert!(matches!(load_from_bytes_interruptible(&bytes, &LoadOptions::default(), &ctl), Err(FormatError::Cancelled(_))));
}

#[test]
fn cancellation_during_blob_inflation_discards_document() {
    let (doc, bytes) = bundle();
    let checks = AtomicUsize::new(0);
    let count = || {
        checks.fetch_add(1, Ordering::Relaxed);
        false
    };
    assert_eq!(load_from_bytes_interruptible(&bytes, &LoadOptions::default(), &Interrupt::cancel_only(&count)).unwrap(), doc);
    let total = checks.load(Ordering::Relaxed);
    // The 8 MiB blob must be checked throughout inflation, not just at its boundaries.
    assert!(total > 128, "only {total} checks");
    checks.store(0, Ordering::Relaxed);
    let interrupted_at = std::sync::Mutex::new(None);
    let cancel = || {
        let cancelled = checks.fetch_add(1, Ordering::Relaxed) >= total / 2;
        if cancelled {
            interrupted_at.lock().unwrap().get_or_insert_with(std::time::Instant::now);
        }
        cancelled
    };
    assert!(matches!(load_from_bytes_interruptible(&bytes, &LoadOptions::default(), &Interrupt::cancel_only(&cancel)), Err(FormatError::Cancelled(_))));
    eprintln!("pcraft cancellation-to-return: {:?}", interrupted_at.lock().unwrap().unwrap().elapsed());
}

#[test]
fn explicit_budgets_return_errors() {
    let (_, bytes) = bundle();
    for opts in [
        LoadOptions { max_manifest_bytes: 1, ..Default::default() },
        LoadOptions { max_blob_bytes: 1 << 20, ..Default::default() },
        LoadOptions { max_total_bytes: 1 << 20, ..Default::default() },
        LoadOptions { max_canvas_pixels: 1, ..Default::default() },
        LoadOptions { max_canvas_side: 1, ..Default::default() },
    ] {
        assert!(matches!(load_from_bytes_interruptible(&bytes, &opts, &Interrupt::NONE), Err(FormatError::LimitExceeded(_))));
    }
}

#[test]
fn cancellation_from_another_thread_returns_no_document() {
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;
    let mut doc = Document::new("large blob", Size::new(6000, 4000), ColorMode::Rgb, SampleType::U8);
    doc.icc_profile = Some(Arc::new(vec![42; 64 << 20]));
    let bytes = save_to_bytes(&doc, &SaveOptions::default()).unwrap();
    let cancelled = AtomicBool::new(false);
    let checks = AtomicUsize::new(0);
    let rendezvous = std::sync::Barrier::new(2);
    let flipped = std::sync::Mutex::new(None);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            rendezvous.wait();
            *flipped.lock().unwrap() = Some(Instant::now());
            cancelled.store(true, Ordering::Release);
            rendezvous.wait();
        });
        let callback = || {
            if checks.fetch_add(1, Ordering::Relaxed) == 8 {
                rendezvous.wait();
                rendezvous.wait();
            }
            cancelled.load(Ordering::Acquire)
        };
        assert!(matches!(load_from_bytes_interruptible(&bytes, &LoadOptions::default(), &Interrupt::cancel_only(&callback)), Err(FormatError::Cancelled(_))));
        eprintln!("pcraft external cancellation-to-return at rendezvous (64 MiB blob): {:?}", flipped.lock().unwrap().unwrap().elapsed());
    });
}

#[test]
fn every_checkpoint_discards_a_partially_loaded_document() {
    let mut doc = crate::common::rich_doc(ColorMode::Rgb, SampleType::U8);
    doc.icc_profile = Some(Arc::new(vec![42; 128 << 10]));
    let bytes = save_to_bytes(&doc, &SaveOptions::default()).unwrap();
    let checks = AtomicUsize::new(0);
    let count = || {
        checks.fetch_add(1, Ordering::Relaxed);
        false
    };
    assert_eq!(load_from_bytes_interruptible(&bytes, &LoadOptions::default(), &Interrupt::cancel_only(&count)).unwrap(), doc);
    let total = checks.load(Ordering::Relaxed);
    for stop in 0..total {
        checks.store(0, Ordering::Relaxed);
        let cancel = || checks.fetch_add(1, Ordering::Relaxed) >= stop;
        assert!(
            matches!(load_from_bytes_interruptible(&bytes, &LoadOptions::default(), &Interrupt::cancel_only(&cancel)), Err(FormatError::Cancelled(_))),
            "checkpoint {stop}/{total}"
        );
    }
}
