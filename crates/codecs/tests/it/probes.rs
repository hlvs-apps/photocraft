use std::sync::atomic::{AtomicUsize, Ordering};

use photocraft_codecs::{
    ChannelLayout, CodecError, EncodeOptions, Format, Image, Limits, ProbeControl, SampleType, decode, encode, probe_size, probe_size_with,
};

#[test]
fn additional_formats_report_dimensions_without_pixel_data() {
    let image = Image::new(7, 5, ChannelLayout::Rgb, SampleType::U8).unwrap();
    for format in [Format::Pnm, Format::Dds, Format::Fits, Format::Dpx, Format::Cineon, Format::Iff] {
        let bytes = encode(&image, format, &EncodeOptions::default()).unwrap();
        let expected = decode(&bytes).unwrap().dimensions();
        assert_eq!(probe_size(&bytes).unwrap(), expected, "{format:?}");
        let prefix = (1..bytes.len()).find(|&n| probe_size(&bytes[..n]).ok() == Some(expected) && decode(&bytes[..n]).is_err());
        assert!(prefix.is_some(), "{format:?}: probing must not require the pixel payload");
    }
}

#[test]
fn probe_limits_and_preset_cancellation_precede_parsing() {
    let bytes = b"P6\n7 5\n255\n";
    assert_eq!(probe_size(bytes).unwrap(), (7, 5));
    assert!(decode(bytes).is_err());
    for ctl in [
        ProbeControl { max_input_bytes: 1, ..Default::default() },
        ProbeControl { limits: Limits { max_pixels: 1, ..Default::default() }, ..Default::default() },
    ] {
        assert!(matches!(probe_size_with(bytes, &ctl), Err(CodecError::LimitExceeded(_))));
    }
    assert!(matches!(probe_size_with(b"not a header", &ProbeControl { cancelled: &|| true, ..Default::default() }), Err(CodecError::Cancelled)));
}

#[test]
fn long_text_headers_cancel_inside_comments_and_escapes() {
    let payload = vec![b'x'; 1 << 20];
    let cases = [
        [b"P6\n#".as_slice(), &payload, b"\n7 5\n255\n"].concat(),
        [b"/* XPM */ /*".as_slice(), &payload, b"*/\n\"7 5 1 1\""].concat(),
        [b"/* XPM */ //".as_slice(), &payload, b"\n\"7 5 1 1\""].concat(),
        [b"! XPM2\n!".as_slice(), &payload, b"\n7 5 1 1\n"].concat(),
        [b"/* XPM */ \"".as_slice(), &b"\\x".repeat(1 << 19), b"\""].concat(),
    ];
    for bytes in cases {
        let calls = AtomicUsize::new(0);
        let cancel = || calls.fetch_add(1, Ordering::Relaxed) >= 5;
        assert!(matches!(probe_size_with(&bytes, &ProbeControl { cancelled: &cancel, ..Default::default() }), Err(CodecError::Cancelled)));
    }
}

#[test]
fn reader_backed_probe_preserves_cancellation() {
    let image = Image::new(7, 5, ChannelLayout::Rgb, SampleType::U8).unwrap();
    for format in [Format::WebP, Format::Bmp, Format::OpenExr, Format::Gif] {
        let bytes = encode(&image, format, &Default::default()).unwrap();
        let calls = AtomicUsize::new(0);
        let cancel = || calls.fetch_add(1, Ordering::Relaxed) >= 3;
        assert!(matches!(probe_size_with(&bytes, &ProbeControl { cancelled: &cancel, ..Default::default() }), Err(CodecError::Cancelled)), "{format:?}");
    }
}
