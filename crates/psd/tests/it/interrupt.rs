use std::sync::atomic::{AtomicUsize, Ordering};

use photocraft_psd::{Compression, LoadOptions, PlaneLayout, PsdError, PsdFile, ReadControl, Version, compression, testgen};

#[test]
fn parser_cancellation_is_not_recovered_as_damage() {
    let bytes = testgen::small(Version::Psd, Compression::Zip).to_bytes().unwrap();
    for after in [0, 10, 100] {
        let calls = AtomicUsize::new(0);
        let cancel = || calls.fetch_add(1, Ordering::Relaxed) >= after;
        let ctl = ReadControl { cancelled: &cancel, ..Default::default() };
        assert!(matches!(PsdFile::from_bytes_controlled(&bytes, |_| true, |_| true, &ctl), Err(PsdError::Cancelled)));
    }
}

#[test]
fn parser_refuses_each_explicit_budget() {
    let bytes = testgen::small(Version::Psd, Compression::Raw).to_bytes().unwrap();
    for limits in [
        LoadOptions { max_input_bytes: 1, ..Default::default() },
        LoadOptions { max_pixels: 1, ..Default::default() },
        LoadOptions { max_layers: 0, ..Default::default() },
        LoadOptions { max_decoded_bytes: 1, ..Default::default() },
        LoadOptions { max_total_decoded_bytes: 1, ..Default::default() },
    ] {
        let ctl = ReadControl { limits, ..Default::default() };
        assert!(matches!(PsdFile::from_bytes_controlled(&bytes, |_| true, |_| true, &ctl), Err(PsdError::LimitExceeded(_))));
    }
}

#[test]
fn every_compression_can_be_cancelled_after_starting() {
    let layout = PlaneLayout { planes: 1, width: 4096, height: 4096, depth: 8, version: Version::Psd };
    let pixels = vec![42; layout.width * layout.height];
    for compression in Compression::ALL {
        let bytes = compression::encode_planes(compression, &pixels, &layout).unwrap();
        let checks = AtomicUsize::new(0);
        let cancelled_at = std::sync::Mutex::new(None);
        let cancel = || {
            let n = checks.fetch_add(1, Ordering::Relaxed);
            // Flip immediately after a false sample, so the measurement includes the next
            // complete work chunk rather than only error propagation from a true sample.
            if n == 20 {
                *cancelled_at.lock().unwrap() = Some(std::time::Instant::now());
            }
            n > 20
        };
        let ctl = ReadControl { cancelled: &cancel, ..Default::default() };
        assert_eq!(compression::decode_planes_with(compression, &bytes, &layout, &ctl), Err(PsdError::Cancelled));
        eprintln!("PSD {compression:?} cancellation-to-return {:?}", cancelled_at.lock().unwrap().unwrap().elapsed());
        let tiny = ReadControl { limits: LoadOptions { max_decoded_bytes: 1, ..Default::default() }, ..Default::default() };
        assert!(matches!(compression::decode_planes_with(compression, &bytes, &layout, &tiny), Err(PsdError::LimitExceeded(_))));
    }
}

#[test]
fn compressed_no_output_work_is_interruptible() {
    // A row with many PackBits no-op packets before its first pixel.
    let mut bytes = (100_002u32).to_be_bytes().to_vec();
    bytes.extend(std::iter::repeat_n(128, 100_000));
    bytes.extend([0, 42]);
    let layout = PlaneLayout { planes: 1, width: 1, height: 1, depth: 8, version: Version::Psb };
    let checks = AtomicUsize::new(0);
    let ctl = ReadControl { cancelled: &|| checks.fetch_add(1, Ordering::Relaxed) > 20, ..Default::default() };
    assert_eq!(compression::decode_planes_with(Compression::Rle, &bytes, &layout, &ctl), Err(PsdError::Cancelled));
}

#[test]
fn explicit_limits_admit_merged_layout_even_without_decoding() {
    let mut bytes = testgen::small(Version::Psb, Compression::Zip).to_bytes().unwrap();
    bytes[14..18].copy_from_slice(&300_000u32.to_be_bytes());
    bytes[18..22].copy_from_slice(&300_000u32.to_be_bytes());
    // Legacy parsing preserves an oversized merged stream opaquely.
    assert!(PsdFile::from_bytes(&bytes).is_ok());
    let control = ReadControl { limits: LoadOptions { max_input_bytes: bytes.len(), ..Default::default() }, ..Default::default() };
    assert!(matches!(PsdFile::from_bytes_controlled(&bytes, |_| false, |_| true, &control), Err(PsdError::LimitExceeded(_))));
}

#[test]
fn default_global_layer_recovery_keeps_structural_limits_recoverable() {
    let mut file = testgen::layered(Version::Psd, photocraft_psd::ColorMode::Rgb, 16, Compression::Raw);
    file.layer_info = None;
    let mut malformed = 1i16.to_be_bytes().to_vec();
    malformed.extend([0; 16]);
    malformed.extend(u16::MAX.to_be_bytes());
    file.global_blocks.push(photocraft_psd::TaggedBlock::new(*b"Lr16", malformed));
    let bytes = file.to_bytes().unwrap();
    let recovered = PsdFile::from_bytes(&bytes).unwrap();
    assert!(recovered.layers().is_empty());
    assert!(recovered.unreadable_layer_block().is_some());
}

#[test]
fn empty_zlib_blocks_do_not_starve_cancellation() {
    let mut data = vec![0x78, 0x01];
    data.extend([0, 0, 0, 0xff, 0xff].repeat(100_000));
    data.extend([1, 0, 0, 0xff, 0xff, 0, 0, 0, 1]);
    let layout = PlaneLayout { planes: 1, width: 0, height: 0, depth: 8, version: Version::Psd };
    assert!(compression::decode_planes(Compression::Zip, &data, &layout).unwrap().is_empty());
    let calls = AtomicUsize::new(0);
    let cancel = || calls.fetch_add(1, Ordering::Relaxed) >= 3;
    assert_eq!(
        compression::decode_planes_with(Compression::Zip, &data, &layout, &ReadControl { cancelled: &cancel, ..Default::default() }),
        Err(PsdError::Cancelled)
    );
}

#[test]
fn converted_plane_checks_cancel_and_output_budget_before_allocation() {
    let cancelled = ReadControl { cancelled: &|| true, ..Default::default() };
    assert_eq!(photocraft_psd::pixels::plane_to_u8_with(&[], 1, usize::MAX, usize::MAX, &cancelled), Err(PsdError::Cancelled));
    for limits in [LoadOptions { max_pixels: 1, ..Default::default() }, LoadOptions { max_decoded_bytes: 1, ..Default::default() }] {
        assert!(matches!(
            photocraft_psd::pixels::plane_to_u8_with(&[0], 1, 8, 1, &ReadControl { limits, ..Default::default() }),
            Err(PsdError::LimitExceeded(_))
        ));
    }
}
