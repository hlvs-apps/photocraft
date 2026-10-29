//! Embedded smart-object files from the PSD global `lnk2`/`lnk3`/`lnkD` blocks.
//!
//! PSD import keeps a placed layer's source as `SmartSource::Linked { path: <Idnt uuid> }` and the
//! file bytes stay inside the preserved global block (so PSD export stays byte-stable). This module
//! finds the bytes for a uuid when the engine needs to re-render the smart object from its source.
//!
//! Layout (Adobe PSD spec, "Linked Layer"): a sequence of items, each `u64 length` + item data,
//! padded to 4 bytes. Item: type (`liFD` data / `liFE` external / `liFA` alias), version (1–7),
//! Pascal uuid, Unicode file name, file type, creator, `u64` data length, open-descriptor flag (+
//! versioned descriptor), then for `liFD` the raw file bytes.

use photocraft_doc::Metadata;
use photocraft_psd::descriptor::VersionedDescriptor;

/// An embedded file found in a linked-layer block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkedFile {
    pub uuid: String,
    pub file_name: String,
    pub bytes: Vec<u8>,
}

struct Rd<'a, 'c> {
    d: &'a [u8],
    p: usize,
    ctl: photocraft_codecs::ProbeControl<'c>,
}

impl<'a> Rd<'a, '_> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        self.ctl.check().ok()?;
        let s = self.d.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_be_bytes(b.try_into().unwrap_or([0; 8])))
    }
    fn unicode(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let raw = self.take(n.checked_mul(2)?)?;
        let units = raw.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c));
        let mut out = String::new();
        for (i, c) in char::decode_utf16(units).enumerate() {
            if i.is_multiple_of(16384) {
                self.ctl.check().ok()?;
            }
            out.push(c.unwrap_or(char::REPLACEMENT_CHARACTER));
        }
        Some(out.trim_end_matches('\0').to_string())
    }
}

/// Parses one item; `None` for malformed data or non-`liFD` items.
fn parse_item(item: &[u8]) -> Option<LinkedFile> {
    let (uuid, file_name, start, len) = item_header(item)?;
    let bytes = item.get(start..start.checked_add(len)?)?.to_vec();
    Some(LinkedFile { uuid, file_name, bytes })
}

/// The uuid and file name of an embedded (`liFD`) item, and where its file bytes start and how
/// many there are; `None` for malformed data or other items.
fn item_header(item: &[u8]) -> Option<(String, String, usize, usize)> {
    item_header_with(item, &photocraft_codecs::ProbeControl::default())
}

fn item_header_with(item: &[u8], ctl: &photocraft_codecs::ProbeControl) -> Option<(String, String, usize, usize)> {
    let mut r = Rd { d: item, p: 0, ctl: *ctl };
    let kind = r.take(4)?;
    let _version = r.u32()?;
    let n = r.u8()? as usize;
    let uuid = String::from_utf8_lossy(r.take(n)?).to_string();
    let file_name = r.unicode()?;
    r.take(8)?; // file type + creator
    let len = usize::try_from(r.u64()?).ok()?;
    if r.u8()? != 0 {
        let (_, used) =
            VersionedDescriptor::parse_prefix_with(item.get(r.p..)?, &photocraft_psd::ReadControl { cancelled: ctl.cancelled, ..Default::default() }).ok()?;
        r.p += used;
    }
    if kind != b"liFD" {
        return None;
    }
    Some((uuid, file_name, r.p, len))
}

/// The file name of the embedded file for smart-object `uuid` among the document's preserved PSD
/// global blocks, without copying its bytes: whether a placed layer's source is embedded.
pub fn embedded_file_name(meta: &Metadata, uuid: &str) -> Option<String> {
    if uuid.is_empty() {
        return None;
    }
    meta.psd_global_blocks.iter().filter(|(_, k, _)| matches!(k, b"lnk2" | b"lnk3" | b"lnkD")).find_map(|(_, _, d)| {
        let mut r = Rd { d, p: 0, ctl: photocraft_codecs::ProbeControl::default() };
        while let Some(len) = r.u64() {
            let item = r.take(usize::try_from(len).ok()?)?;
            if let Some((_, name, ..)) = item_header(item).filter(|h| h.0 == uuid) {
                return Some(name);
            }
            r.p = r.p.next_multiple_of(4);
        }
        None
    })
}

/// Every embedded (`liFD`) file in one linked-layer block's data.
pub fn parse_linked_files(data: &[u8]) -> Vec<LinkedFile> {
    let mut out = Vec::new();
    let mut r = Rd { d: data, p: 0, ctl: photocraft_codecs::ProbeControl::default() };
    while let Some(len) = r.u64() {
        let Ok(len) = usize::try_from(len) else { break };
        let Some(item) = r.take(len) else { break };
        out.extend(parse_item(item));
        r.p = r.p.next_multiple_of(4);
    }
    out
}

/// The embedded file for smart-object `uuid` among the document's preserved PSD global blocks.
pub fn find_linked_file(meta: &Metadata, uuid: &str) -> Option<LinkedFile> {
    find_in_blocks(meta.psd_global_blocks.iter().map(|(_, k, d)| (k, d.as_slice())), uuid)
}

/// The embedded file for smart-object `uuid` among PSD global blocks given as (key, data).
pub fn find_in_blocks<'a>(blocks: impl IntoIterator<Item = (&'a [u8; 4], &'a [u8])>, uuid: &str) -> Option<LinkedFile> {
    if uuid.is_empty() {
        return None;
    }
    let (uuid, file_name, bytes) = find_slice_in_blocks(blocks, uuid)?;
    Some(LinkedFile { uuid, file_name, bytes: bytes.to_vec() })
}

/// [`find_in_blocks`] without copying: the uuid, file name and bytes of the embedded file, as a
/// slice of the block. Only the matching item's header is parsed beyond the uuid, and no other
/// embedded file is copied (a `lnk2` block can hold gigabytes of them).
type LinkedSlice<'a> = (String, String, &'a [u8]);

fn find_slice_in_blocks<'a>(blocks: impl IntoIterator<Item = (&'a [u8; 4], &'a [u8])>, uuid: &str) -> Option<LinkedSlice<'a>> {
    find_slice_in_blocks_with(blocks, uuid, &photocraft_codecs::ProbeControl::default()).ok().flatten()
}

fn find_slice_in_blocks_with<'a>(
    blocks: impl IntoIterator<Item = (&'a [u8; 4], &'a [u8])>,
    uuid: &str,
    ctl: &photocraft_codecs::ProbeControl,
) -> Result<Option<LinkedSlice<'a>>, photocraft_codecs::CodecError> {
    if uuid.is_empty() {
        return Ok(None);
    }
    for (_, d) in blocks.into_iter().filter(|(k, _)| matches!(*k, b"lnk2" | b"lnk3" | b"lnkD")) {
        ctl.check()?;
        if d.len() > ctl.max_input_bytes {
            return Err(photocraft_codecs::CodecError::LimitExceeded("linked block input byte budget".into()));
        }
        let mut r = Rd { d, p: 0, ctl: *ctl };
        while let Some(len) = r.u64() {
            ctl.check()?;
            let Ok(len) = usize::try_from(len) else { break };
            let Some(item) = r.take(len) else { break };
            let found =
                item_header_with(item, ctl).filter(|h| h.0 == uuid).and_then(|(id, name, start, n)| Some((id, name, item.get(start..start.checked_add(n)?)?)));
            ctl.check()?;
            if found.is_some() {
                return Ok(found);
            }
            r.p = r.p.next_multiple_of(4);
        }
        ctl.check()?;
    }
    Ok(None)
}

/// The size of embedded file `uuid` among PSD global blocks given as (key, data), when it is there
/// and readable with a non-empty size (the source size a `PlLd` record leaves out).
pub(crate) fn source_size_in_blocks<'a>(
    blocks: impl IntoIterator<Item = (&'a [u8; 4], &'a [u8])>,
    uuid: &str,
    ctl: &photocraft_raster::Interrupt,
) -> Option<(f64, f64)> {
    source_size_in_blocks_with(blocks, uuid, photocraft_psd::LoadOptions::default(), ctl).ok().flatten()
}

pub(crate) fn source_size_in_blocks_with<'a>(
    blocks: impl IntoIterator<Item = (&'a [u8; 4], &'a [u8])>,
    uuid: &str,
    limits: photocraft_psd::LoadOptions,
    interrupt: &photocraft_raster::Interrupt,
) -> Result<Option<(f64, f64)>, photocraft_codecs::CodecError> {
    let ctl = photocraft_codecs::ProbeControl {
        limits: photocraft_codecs::Limits { max_pixels: limits.max_pixels, max_alloc: limits.max_decoded_bytes, ..photocraft_codecs::Limits::none() },
        max_input_bytes: limits.max_input_bytes,
        cancelled: &|| interrupt.cancelled(),
    };
    let Some((_, _, bytes)) = find_slice_in_blocks_with(blocks, uuid, &ctl)? else { return Ok(None) };
    source_size_probe_with(bytes, &ctl)
}

#[cfg(test)]
pub(crate) fn source_size_probe(bytes: &[u8]) -> Option<(f64, f64)> {
    source_size_probe_with(bytes, &photocraft_codecs::ProbeControl::default()).ok().flatten()
}

/// Read placement dimensions without decoding pixels. PSD/PNG/JPEG/TIFF retain their
/// stored-size semantics; other formats use their codec's upright header dimensions.
fn source_size_probe_with(bytes: &[u8], ctl: &photocraft_codecs::ProbeControl) -> Result<Option<(f64, f64)>, photocraft_codecs::CodecError> {
    use photocraft_codecs::CodecError;
    ctl.check()?;
    if bytes.len() > ctl.max_input_bytes {
        return Err(CodecError::LimitExceeded("embedded input byte budget".into()));
    }
    let be32 = |at: usize| bytes.get(at..at + 4).and_then(|b| b.try_into().ok()).map(u32::from_be_bytes);
    let result: Result<Option<(u32, u32)>, CodecError> = match bytes {
        _ if crate::is_psd(bytes) => Ok(be32(18).zip(be32(14))),
        [b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..] | [b'I', b'I', 43, 0, ..] | [b'M', b'M', 0, 43, ..] => {
            photocraft_codecs::tiff_info_with(bytes, ctl).map(|info| info.default_page().and_then(|i| info.pages.get(i)).map(|p| (p.width, p.height)))
        }
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Ok(be32(16).zip(be32(20))),
        [0xff, 0xd8, ..] => jpeg_size_with(bytes, ctl).map(|size| size.map(|(w, h)| (w as u32, h as u32))),
        _ => photocraft_codecs::probe_size_with(bytes, ctl).map(Some),
    };
    ctl.check()?;
    match result {
        Ok(Some((w, h))) if w > 0 && h > 0 => {
            ctl.limits.check_dimensions(w, h)?;
            Ok(Some((f64::from(w), f64::from(h))))
        }
        Err(e @ (CodecError::Cancelled | CodecError::LimitExceeded(_))) => Err(e),
        _ => Ok(None),
    }
}

/// A JPEG's size from its first start-of-frame marker (ITU-T T.81 B.2.2), walking the marker
/// segments without decoding. Each step moves forward, so a malformed file ends the walk.
#[cfg(test)]
fn jpeg_size(bytes: &[u8]) -> Option<(f64, f64)> {
    jpeg_size_with(bytes, &photocraft_codecs::ProbeControl::default()).ok().flatten()
}

fn jpeg_size_with(bytes: &[u8], ctl: &photocraft_codecs::ProbeControl) -> Result<Option<(f64, f64)>, photocraft_codecs::CodecError> {
    let result = jpeg_size_checked(bytes, ctl);
    ctl.check()?;
    Ok(result)
}

fn jpeg_size_checked(bytes: &[u8], ctl: &photocraft_codecs::ProbeControl) -> Option<(f64, f64)> {
    let mut at = 2usize;
    loop {
        ctl.check().ok()?;
        // Markers are 0xFF then a code; extra 0xFF fill bytes may come first.
        while bytes.get(at) == Some(&0xFF) && bytes.get(at + 1) == Some(&0xFF) {
            if at.is_multiple_of(64 << 10) {
                ctl.check().ok()?;
            }
            at += 1;
        }
        if *bytes.get(at)? != 0xFF {
            return None;
        }
        let code = *bytes.get(at + 1)?;
        match code {
            // SOF0..SOF15, except DHT (C4), JPG (C8) and DAC (CC).
            0xC0..=0xCF if !matches!(code, 0xC4 | 0xC8 | 0xCC) => {
                let h = bytes.get(at + 5..at + 7)?;
                let w = bytes.get(at + 7..at + 9)?;
                return Some((f64::from(u16::from_be_bytes([w[0], w[1]])), f64::from(u16::from_be_bytes([h[0], h[1]]))));
            }
            // Standalone markers have no length.
            0x01 | 0xD0..=0xD7 => at = at.checked_add(2)?,
            // Start of scan or end of image before any frame header.
            0xDA | 0xD9 => return None,
            _ => {
                let len = bytes.get(at + 2..at + 4).map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))?;
                if len < 2 {
                    return None;
                }
                at = at.checked_add(2 + len)?;
            }
        }
    }
}

/// Size and resolution of a smart-object source file (PSD/PSB header, `.pcraft`, or any image).
/// Other formats are decoded in full, which `ctl` can cancel (its progress is left alone: this is
/// a step of a larger operation).
pub(crate) fn source_geometry(name: &str, bytes: &[u8], ctl: &photocraft_raster::Interrupt) -> Result<((f64, f64), f64), String> {
    if crate::is_psd(bytes) {
        let rd = |at: usize| bytes.get(at..at + 4).map(|b| f64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])));
        let (h, w) = rd(14).zip(rd(18)).ok_or("truncated PSD header")?;
        return Ok(((w, h), 72.0));
    }
    let cancel = || ctl.cancelled();
    let doc = crate::import_with(name, bytes, &photocraft_raster::Interrupt::cancel_only(&cancel)).map_err(|e| e.to_string())?.document;
    Ok(((f64::from(doc.size.width), f64::from(doc.size.height)), f64::from(doc.resolution_dpi)))
}

/// The four-character file type Photoshop records for an embedded file, from its contents.
pub fn file_type(bytes: &[u8]) -> [u8; 4] {
    match bytes {
        [b'8', b'B', b'P', b'S', 0, 2, ..] => *b"8BPB",
        [b'8', b'B', b'P', b'S', ..] => *b"8BPS",
        [0x89, b'P', b'N', b'G', ..] => *b"PNGf",
        [0xFF, 0xD8, ..] => *b"JPEG",
        [b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..] => *b"TIFF",
        [b'G', b'I', b'F', ..] => *b"GIFf",
        _ => *b"    ",
    }
}

/// Encodes one `liFD` item (version 7: no open descriptor, empty child document id, no
/// modification time, unlocked), length-prefixed and padded to 4 bytes.
pub fn encode_linked_file(f: &LinkedFile) -> Vec<u8> {
    let mut item = Vec::new();
    item.extend_from_slice(b"liFD");
    item.extend_from_slice(&7u32.to_be_bytes());
    let id = f.uuid.as_bytes().get(..f.uuid.len().min(255)).unwrap_or_default();
    item.push(id.len() as u8);
    item.extend_from_slice(id);
    // NUL-terminated, the terminator counted (as Photoshop writes and reads it).
    let units: Vec<u16> = f.file_name.encode_utf16().chain(std::iter::once(0)).collect();
    item.extend_from_slice(&(units.len() as u32).to_be_bytes());
    for u in units {
        item.extend_from_slice(&u.to_be_bytes());
    }
    item.extend_from_slice(&file_type(&f.bytes));
    item.extend_from_slice(b"8BIM");
    item.extend_from_slice(&(f.bytes.len() as u64).to_be_bytes());
    item.push(0);
    item.extend_from_slice(&f.bytes);
    item.extend_from_slice(&0u32.to_be_bytes()); // child document id (version 5)
    item.extend_from_slice(&0f64.to_be_bytes()); // asset modification time (version 6)
    item.push(0); // asset locked (version 7)
    let mut out = (item.len() as u64).to_be_bytes().to_vec();
    out.extend_from_slice(&item);
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
    out
}

/// The uuid of one linked-layer item (any type), if its header is readable.
fn item_uuid(item: &[u8]) -> Option<String> {
    let n = usize::from(*item.get(8)?);
    Some(String::from_utf8_lossy(item.get(9..9 + n)?).into_owned())
}

/// The items of a linked-layer block: each item's uuid (when readable) and its bytes including
/// the length prefix and padding. A malformed tail is returned as one unnamed piece.
fn split_items(data: &[u8]) -> Vec<(Option<String>, &[u8])> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(rest) = data.get(at..).filter(|r| !r.is_empty()) {
        let len = rest.get(..8).map(|b| u64::from_be_bytes(b.try_into().unwrap_or([0; 8])));
        let end = len.and_then(|l| usize::try_from(l).ok()).and_then(|l| l.checked_add(8)).filter(|&e| e <= rest.len());
        let (Some(end), Some(item)) = (end, end.and_then(|e| rest.get(8..e))) else {
            out.push((None, rest));
            break;
        };
        let padded = end.next_multiple_of(4).min(rest.len());
        out.push((item_uuid(item), rest.get(..padded).unwrap_or(rest)));
        at += padded;
    }
    out
}

/// Rewrites a linked-layer block: keeps the items `keep` accepts (and anything unreadable), then
/// appends `add`. `None` when nothing would change.
pub fn rebuild_block(data: &[u8], keep: &dyn Fn(&str) -> bool, add: &[LinkedFile]) -> Option<Vec<u8>> {
    let items = split_items(data);
    let dropped = items.iter().any(|(u, _)| u.as_deref().is_some_and(|u| !keep(u)));
    if !dropped && add.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    for (u, bytes) in items {
        if u.as_deref().is_none_or(keep) {
            out.extend_from_slice(bytes);
        }
    }
    for f in add {
        out.extend(encode_linked_file(f));
    }
    Some(out)
}

/// The uuids of every item in a linked-layer block.
pub fn block_uuids(data: &[u8]) -> Vec<String> {
    split_items(data).into_iter().filter_map(|(u, _)| u).collect()
}

/// Copies only records accepted by `keep` (UUID and complete raw record including padding).
/// Unlike [`rebuild_block`], unidentified records and malformed tails are discarded: clipboard
/// copies must not carry unrelated embedded files. Empty UUIDs cannot identify source bytes.
/// Selected identity must be provable without lossy UTF-8 decoding; an ambiguous or truncated
/// selected ID is an error instead. No payload decoding or filesystem access is performed.
/// `None` means the block already contains only selected complete records and can be shared.
pub fn select_items(data: &[u8], keep: &dyn Fn(&str, &[u8]) -> bool) -> Result<Option<Vec<u8>>, crate::IoError> {
    let error = || crate::IoError::Unsupported("can't copy a truncated selected PSD linked-layer record".into());
    let mut selected = None;
    let mut at = 0usize;
    while let Some(rest) = data.get(at..).filter(|r| !r.is_empty()) {
        let Some((item, raw)) = framed_item(rest) else {
            if let Some(item) = rest.get(8..)
                && selection_uuid(item, rest, keep)?.is_some_and(|uuid| keep(uuid, rest))
            {
                return Err(error());
            }
            if selected.is_none() {
                let mut prefix = Vec::new();
                append_selected(&mut prefix, data.get(..at).ok_or_else(error)?)?;
                selected = Some(prefix);
            }
            break;
        };
        if selection_uuid(item, raw, keep)?.is_some_and(|uuid| keep(uuid, raw)) {
            if let Some(out) = &mut selected {
                append_selected(out, raw)?;
            }
        } else if selected.is_none() {
            let mut prefix = Vec::new();
            append_selected(&mut prefix, data.get(..at).ok_or_else(error)?)?;
            selected = Some(prefix);
        }
        at = at.checked_add(raw.len()).ok_or_else(error)?;
    }
    Ok(selected)
}

// Legacy PSD readers retain lossy IDs for file fidelity. Selection needs exact identity;
// a lossy label is used only to refuse an ambiguous transfer, never to retain a record.
fn selection_uuid<'a>(item: &'a [u8], raw: &[u8], keep: &dyn Fn(&str, &[u8]) -> bool) -> Result<Option<&'a str>, crate::IoError> {
    let Some(n) = item.get(8).map(|n| usize::from(*n)) else { return Ok(None) };
    let Some(bytes) = 9usize.checked_add(n).and_then(|end| item.get(9..end)).filter(|bytes| !bytes.is_empty()) else { return Ok(None) };
    match std::str::from_utf8(bytes) {
        Ok(uuid) => Ok(Some(uuid)),
        Err(_) if keep(&String::from_utf8_lossy(bytes), raw) => {
            Err(crate::IoError::Unsupported("can't verify a PSD linked-layer source ID with invalid UTF-8".into()))
        }
        Err(_) => Ok(None),
    }
}

/// Whether a complete raw record already occurs in a block, without decoding its file bytes.
pub fn contains_item(data: &[u8], record: &[u8]) -> bool {
    let mut remaining = data;
    while let Some((_, raw)) = framed_item(remaining) {
        if raw == record {
            return true;
        }
        let Some(next) = remaining.get(raw.len()..) else { return false };
        remaining = next;
    }
    false
}

fn framed_item(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let len = usize::try_from(u64::from_be_bytes(data.get(..8)?.try_into().ok()?)).ok()?;
    let end = len.checked_add(8)?;
    let item = data.get(8..end)?;
    let padded = end.checked_add(3).map(|n| n & !3)?.min(data.len());
    Some((item, data.get(..padded)?))
}

fn append_selected(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), crate::IoError> {
    out.try_reserve(bytes.len()).map_err(|_| crate::IoError::Unsupported("not enough memory to copy PSD linked-layer records".into()))?;
    out.extend_from_slice(bytes);
    Ok(())
}

/// Copies one linked item under a new UUID, preserving its embedded, external or alias payload.
pub(crate) fn copy_item(data: &[u8], uuid: &str, new_uuid: &str) -> Option<Vec<u8>> {
    let (_, raw) = split_items(data).into_iter().find(|(u, _)| u.as_deref() == Some(uuid))?;
    let len = usize::try_from(u64::from_be_bytes(raw.get(..8)?.try_into().ok()?)).ok()?;
    let item = raw.get(8..8usize.checked_add(len)?)?;
    let old_end = 9usize.checked_add(usize::from(*item.get(8)?))?;
    let new_len = u8::try_from(new_uuid.len()).ok()?;
    let mut copy = item.get(..8)?.to_vec();
    copy.push(new_len);
    copy.extend_from_slice(new_uuid.as_bytes());
    copy.extend_from_slice(item.get(old_end..)?);
    let mut out = (copy.len() as u64).to_be_bytes().to_vec();
    out.extend(copy);
    out.resize(out.len().next_multiple_of(4), 0);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// #2465: a placed source's size comes from its header, for every format a CS2 file embeds,
    /// and never by importing it as a document.
    #[test]
    fn source_size_probe_reads_headers() {
        use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, SampleType};
        let img = Image::new(7, 5, ChannelLayout::Rgb, SampleType::U8).unwrap();
        for format in [Format::Png, Format::Jpeg, Format::Tiff, Format::Bmp, Format::WebP, Format::Gif, Format::OpenExr, Format::Tga] {
            let bytes = photocraft_codecs::encode(&img, format, &EncodeOptions::default()).unwrap();
            assert_eq!(source_size_probe(&bytes), Some((7.0, 5.0)), "{format:?}");
            // A header cut short neither panics nor invents a size (the decode fallback fails too).
            assert_eq!(source_size_probe(&bytes[..10]), None, "{format:?} truncated");
        }
        let mut psd = b"8BPS\0\x01".to_vec();
        psd.extend([0; 6]);
        psd.extend(3u16.to_be_bytes());
        psd.extend(40u32.to_be_bytes()); // height
        psd.extend(30u32.to_be_bytes()); // width
        assert_eq!(source_size_probe(&psd), Some((30.0, 40.0)));
        assert_eq!(source_size_probe(b"not an image"), None);
        assert_eq!(source_size_probe(&[]), None);
    }

    /// Malformed JPEG marker walks end without a size and without panicking or looping.
    #[test]
    fn jpeg_size_rejects_malformed_marker_walks() {
        let sof = [0xFF, 0xC0, 0, 17, 8, 0, 5, 0, 7];
        let ok: Vec<u8> = [&[0xFF, 0xD8][..], &[0xFF, 0xE0, 0, 4, 1, 2], &[0xFF, 0xFF, 0xD0], &sof].concat();
        assert_eq!(jpeg_size(&ok), Some((7.0, 5.0)), "APP0, a fill byte and a standalone marker are skipped");
        for bad in [
            vec![0xFF, 0xD8],
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 1],       // length below 2
            vec![0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF], // length past the end
            vec![0xFF, 0xD8, 0x00, 0x00],             // not a marker
            vec![0xFF, 0xD8, 0xFF, 0xDA, 0, 2],       // scan before any frame header
            vec![0xFF, 0xD8, 0xFF, 0xC0, 0, 17, 8],   // truncated frame header
            [&[0xFF, 0xD8][..], &[0xFF; 64]].concat(),
        ] {
            assert_eq!(jpeg_size(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn round_trips_and_finds_by_uuid() {
        let a = LinkedFile { uuid: "abc-1".into(), file_name: "a.png".into(), bytes: vec![1, 2, 3] };
        let b = LinkedFile { uuid: "def-2".into(), file_name: "b.psd".into(), bytes: vec![9; 10] };
        let mut data = encode_linked_file(&a);
        data.extend(encode_linked_file(&b));
        assert_eq!(parse_linked_files(&data), vec![a.clone(), b.clone()]);
        let mut meta = Metadata::default();
        meta.psd_global_blocks.push((*b"8BIM", *b"lnk2", Arc::new(data)));
        assert_eq!(find_linked_file(&meta, "def-2"), Some(b));
        assert_eq!(find_linked_file(&meta, "zzz"), None);
        assert_eq!(find_linked_file(&meta, ""), None);
    }

    #[test]
    fn blocks_are_pruned_and_extended() {
        let a = LinkedFile { uuid: "a".into(), file_name: "a.png".into(), bytes: vec![0x89, b'P', b'N', b'G', 1] };
        let b = LinkedFile { uuid: "b".into(), file_name: "b.psb".into(), bytes: b"8BPS\0\x02rest".to_vec() };
        let c = LinkedFile { uuid: "c".into(), file_name: "c".into(), bytes: vec![7; 3] };
        let mut data = encode_linked_file(&a);
        data.extend(encode_linked_file(&b));
        assert_eq!(block_uuids(&data), vec!["a", "b"]);
        assert_eq!(rebuild_block(&data, &|_| true, &[]), None);
        let out = rebuild_block(&data, &|u| u != "a", std::slice::from_ref(&c)).unwrap();
        assert_eq!(parse_linked_files(&out), vec![b.clone(), c.clone()]);
        photocraft_psd::TaggedBlock::new(*b"lnk2", out).check_structure().unwrap();
        assert_eq!(file_type(&b.bytes), *b"8BPB");
        assert_eq!(file_type(&a.bytes), *b"PNGf");
        // A malformed tail is kept as is.
        let mut bad = encode_linked_file(&a);
        bad.extend([0, 0, 0, 0, 0, 0, 1, 0, 9]);
        let out = rebuild_block(&bad, &|_| true, std::slice::from_ref(&c)).unwrap();
        assert!(out.starts_with(&bad));
    }

    #[test]
    fn malformed_data_is_ignored() {
        assert!(parse_linked_files(&[0, 0, 0, 0, 0, 0, 0, 200, 1, 2]).is_empty());
        assert!(parse_linked_files(&[]).is_empty());
        let mut d = encode_linked_file(&LinkedFile { uuid: "x".into(), file_name: "f".into(), bytes: vec![5; 4] });
        d.truncate(d.len() - 6);
        assert!(parse_linked_files(&d).is_empty());
    }

    #[test]
    fn copying_linked_item_preserves_unknown_external_payload() {
        let mut raw = encode_linked_file(&LinkedFile { uuid: "old".into(), file_name: "missing.psb".into(), bytes: vec![7; 5] });
        raw[8..12].copy_from_slice(b"liFE");
        let copy = copy_item(&raw, "old", "independent-copy").unwrap();
        assert_eq!(block_uuids(&copy), vec!["independent-copy"]);
        let original_len = u64::from_be_bytes(raw[..8].try_into().unwrap()) as usize;
        let copied_len = u64::from_be_bytes(copy[..8].try_into().unwrap()) as usize;
        assert_eq!(&copy[8..16], &raw[8..16]);
        assert_eq!(&copy[17 + "independent-copy".len()..8 + copied_len], &raw[20..8 + original_len]);
        assert!(copy_item(&raw, "missing", "new").is_none());
    }
    #[test]
    fn selected_records_exclude_other_files_and_unidentified_pieces() {
        let a = encode_linked_file(&LinkedFile { uuid: "a".into(), file_name: "a.psb".into(), bytes: vec![1; 3] });
        let b = encode_linked_file(&LinkedFile { uuid: "b".into(), file_name: "b.psb".into(), bytes: vec![2; 4] });
        assert!(select_items(&a, &|uuid, _| uuid == "a").unwrap().is_none(), "an exclusive block can keep its original Arc");
        let mut data = a.clone();
        data.extend(&b);
        data.extend([0, 0, 0, 0, 0, 0, 1, 0, 9]);
        assert_eq!(select_items(&data, &|uuid, _| uuid == "a").unwrap(), Some(a.clone()));
        assert_eq!(select_items(&data, &|uuid, _| uuid == "b").unwrap(), Some(b));
        assert_eq!(select_items(&data, &|_, _| false).unwrap(), Some(Vec::new()));
        // A complete but unidentified item can be skipped without losing a later selected item.
        let mut unreadable = 1u64.to_be_bytes().to_vec();
        unreadable.extend([7, 0, 0, 0]);
        unreadable.extend(&a);
        assert_eq!(select_items(&unreadable, &|uuid, _| uuid == "a").unwrap(), Some(a));
    }

    #[test]
    fn selected_external_and_alias_records_keep_unknown_payloads_verbatim() {
        for kind in [*b"liFE", *b"liFA", *b"next"] {
            let mut raw = encode_linked_file(&LinkedFile { uuid: "selected".into(), file_name: "missing.psb".into(), bytes: vec![7; 5] });
            raw[8..12].copy_from_slice(&kind);
            raw[12..16].copy_from_slice(&42u32.to_be_bytes()); // an unknown future version
            let mut data = encode_linked_file(&LinkedFile { uuid: "unselected".into(), file_name: "other.psb".into(), bytes: vec![9; 3] });
            data.extend(&raw);
            assert_eq!(select_items(&data, &|uuid, _| uuid == "selected").unwrap(), Some(raw));
        }
    }

    #[test]
    fn truncated_selected_records_fail_instead_of_becoming_unavailable() {
        let mut raw = encode_linked_file(&LinkedFile { uuid: "selected".into(), file_name: "missing.psb".into(), bytes: vec![7; 5] });
        raw[..8].copy_from_slice(&u64::MAX.to_be_bytes());
        assert!(select_items(&raw, &|uuid, _| uuid == "selected").unwrap_err().to_string().contains("truncated selected"));
        assert_eq!(select_items(&raw, &|uuid, _| uuid == "other").unwrap(), Some(Vec::new()));
    }
    #[test]
    fn existing_records_are_matched_individually_without_matching_other_payloads() {
        let a = encode_linked_file(&LinkedFile { uuid: "a".into(), file_name: "a.psb".into(), bytes: vec![1; 3] });
        let b = encode_linked_file(&LinkedFile { uuid: "b".into(), file_name: "b.psb".into(), bytes: vec![2; 4] });
        let different = encode_linked_file(&LinkedFile { uuid: "a".into(), file_name: "a.psb".into(), bytes: vec![9; 3] });
        let mut combined = a.clone();
        combined.extend(&b);
        assert!(contains_item(&combined, &a));
        assert!(contains_item(&combined, &b));
        assert!(!contains_item(&combined, &different));
        assert_eq!(select_items(&a, &|_, raw| !contains_item(&combined, raw)).unwrap(), Some(Vec::new()));
        assert!(select_items(&different, &|_, raw| !contains_item(&combined, raw)).unwrap().is_none());
    }
    #[test]
    fn empty_uuids_are_unidentified_even_when_requested() {
        let raw = encode_linked_file(&LinkedFile { uuid: String::new(), file_name: "unidentified.psb".into(), bytes: vec![7; 5] });
        assert_eq!(select_items(&raw, &|_, _| true).unwrap(), Some(Vec::new()));
        let mut truncated = raw;
        truncated[..8].copy_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(select_items(&truncated, &|_, _| true).unwrap(), Some(Vec::new()));
    }
    #[test]
    fn selection_never_uses_lossy_uuid_aliases_as_record_identity() {
        let mut a = encode_linked_file(&LinkedFile { uuid: "x".into(), file_name: "a.psb".into(), bytes: vec![1; 3] });
        let mut b = encode_linked_file(&LinkedFile { uuid: "x".into(), file_name: "b.psb".into(), bytes: vec![2; 4] });
        a[17] = 0xff;
        b[17] = 0xfe;
        let mut data = a;
        data.extend(b);
        assert_eq!(block_uuids(&data), vec!["\u{fffd}", "\u{fffd}"], "legacy lossy parsing remains unchanged");
        assert!(select_items(&data, &|uuid, _| uuid == "\u{fffd}").unwrap_err().to_string().contains("invalid UTF-8"));
        let valid = encode_linked_file(&LinkedFile { uuid: "valid".into(), file_name: "valid.psb".into(), bytes: vec![3; 5] });
        data.extend(&valid);
        assert_eq!(select_items(&data, &|uuid, _| uuid == "valid").unwrap(), Some(valid));
        // A literal replacement character can itself be a valid UTF-8 identity.
        let valid_unicode = encode_linked_file(&LinkedFile { uuid: "\u{fffd}".into(), file_name: "unicode.psb".into(), bytes: vec![4; 2] });
        assert!(select_items(&valid_unicode, &|uuid, _| uuid == "\u{fffd}").unwrap().is_none());
    }
}
