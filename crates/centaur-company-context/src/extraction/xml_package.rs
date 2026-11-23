//! Repairs the XML parts (`.xml`, `.rels`, and EPUB `.opf`) of an Office,
//! OpenDocument, or EPUB package that pandoc's XML parser rejects but Excel,
//! Word, and PowerPoint open: a leading UTF-8 byte order mark, or NUL padding
//! after the root element. pandoc 3.14 fails such a workbook with "unexpected
//! content after the root XML element".
//!
//! Only single-disk, unencrypted, non-ZIP64 archives whose every entry is
//! stored or deflated are read, within a fixed inflation budget and the
//! extraction deadline, and the repaired package may not grow past a fixed
//! multiple of the original. A package that needs no repair, or that this
//! reader does not handle, is left to pandoc unchanged.

use std::{
    borrow::Cow,
    io::{self, Read, Write},
    num::TryFromIntError,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use flate2::{Compression, Crc, read::DeflateDecoder, write::DeflateEncoder};

const LOCAL_HEADER: u32 = 0x0403_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4b50;
const LOCAL_HEADER_LEN: usize = 30;
const CENTRAL_HEADER_LEN: usize = 46;
const END_OF_CENTRAL_DIRECTORY_LEN: usize = 22;
const STORED: u16 = 0;
const DEFLATED: u16 = 8;
/// Version needed to extract a deflated entry.
const VERSION_NEEDED: u16 = 20;
/// Traditional (bit 0) and strong (bit 6) encryption.
const ENCRYPTED: u16 = 0x0041;
/// Sizes and CRC follow the data in a descriptor; written headers carry them.
const DATA_DESCRIPTOR: u16 = 0x0008;
const UTF8_NAMES: u16 = 0x0800;
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
/// Bytes inflated across all XML parts before the package is left to pandoc,
/// whose own heap is capped at 512 MiB.
const MAX_INFLATED_BYTES: u64 = 512 * 1024 * 1024;
/// A repaired package may grow to twice the original plus this much; repaired
/// parts are deflated again at the fastest level, which compresses less well.
const MAX_GROWTH_BYTES: usize = 1024 * 1024;
const BUFFER_LEN: usize = 64 * 1024;

/// The archive uses a feature this reader does not handle, or is malformed.
#[derive(Debug)]
struct Unsupported;

impl From<io::Error> for Unsupported {
    fn from(_: io::Error) -> Self {
        Self
    }
}

impl From<TryFromIntError> for Unsupported {
    fn from(_: TryFromIntError) -> Self {
        Self
    }
}

/// When a repair gives up: at the extraction deadline, or once its caller has
/// gone away and set `cancelled`.
pub(super) struct Deadline {
    at: Instant,
    cancelled: Arc<AtomicBool>,
}

impl Deadline {
    pub(super) fn new(at: Instant, cancelled: Arc<AtomicBool>) -> Self {
        Self { at, cancelled }
    }

    fn check(&self) -> Result<(), Unsupported> {
        if self.cancelled.load(Ordering::Relaxed) || Instant::now() >= self.at {
            return Err(Unsupported);
        }
        Ok(())
    }
}

/// Returns the package with its XML parts repaired and how many changed, or
/// `None` when no part needs repair, the archive is not read here, or the
/// deadline passes first.
pub(super) fn repair(package: &[u8], deadline: &Deadline) -> Option<(Vec<u8>, usize)> {
    let entries = read_entries(package).ok()?;
    let mut budget = MAX_INFLATED_BYTES;
    let mut repairs = Vec::with_capacity(entries.len());
    for entry in &entries {
        deadline.check().ok()?;
        repairs.push(if is_xml_part(entry.name) {
            inspect(entry, &mut budget, deadline).ok()?
        } else {
            None
        });
    }
    let repaired = repairs.iter().flatten().count();
    if repaired == 0 {
        return None;
    }
    let max_len = package
        .len()
        .saturating_mul(2)
        .saturating_add(MAX_GROWTH_BYTES);
    Some((
        write_package(max_len, &entries, &repairs, deadline).ok()?,
        repaired,
    ))
}

struct Entry<'a> {
    version_made_by: u16,
    version_needed: u16,
    flags: u16,
    method: u16,
    time: u16,
    date: u16,
    crc: u32,
    uncompressed_size: u32,
    internal_attributes: u16,
    external_attributes: u32,
    name: &'a [u8],
    /// Compressed bytes, located through the entry's local header.
    data: &'a [u8],
}

impl Entry<'_> {
    /// Inflated bytes, limited to one past the declared size so that an
    /// understated size is detected rather than trusted.
    fn reader(&self) -> Result<Box<dyn Read + '_>, Unsupported> {
        let limit = u64::from(self.uncompressed_size) + 1;
        match self.method {
            STORED => Ok(Box::new(self.data.take(limit))),
            DEFLATED => Ok(Box::new(DeflateDecoder::new(self.data).take(limit))),
            _ => Err(Unsupported),
        }
    }
}

fn is_xml_part(name: &[u8]) -> bool {
    let name = name.to_ascii_lowercase();
    [&b".xml"[..], b".rels", b".opf"]
        .iter()
        .any(|extension| name.ends_with(extension))
}

fn is_xml_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn is_padding(byte: u8) -> bool {
    byte == b'\0' || is_xml_whitespace(byte)
}

/// Finds how a part starts, in constant memory across chunks: whether it has a
/// UTF-8 byte order mark, and its first two bytes after that mark and any XML
/// whitespace.
#[derive(Default)]
struct Lead {
    /// The first bytes, until a byte order mark can be recognized.
    head: Vec<u8>,
    has_bom: Option<bool>,
    markup: Vec<u8>,
}

impl Lead {
    fn feed(&mut self, mut chunk: &[u8]) {
        if self.has_bom.is_none() {
            let wanted = (UTF8_BOM.len() - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..wanted]);
            chunk = &chunk[wanted..];
            if self.head.len() < UTF8_BOM.len() {
                return;
            }
            self.settle_head();
        }
        self.scan(chunk);
    }

    /// Called once the part has ended, in case it was shorter than a mark.
    fn finish(&mut self) {
        if self.has_bom.is_none() {
            self.settle_head();
        }
    }

    fn settle_head(&mut self) {
        let head = std::mem::take(&mut self.head);
        let has_bom = head == UTF8_BOM;
        self.has_bom = Some(has_bom);
        if !has_bom {
            self.scan(&head);
        }
    }

    fn scan(&mut self, bytes: &[u8]) {
        if self.markup.len() == 2 {
            return;
        }
        let bytes = if self.markup.is_empty() {
            match bytes.iter().position(|byte| !is_xml_whitespace(*byte)) {
                Some(start) => &bytes[start..],
                None => return,
            }
        } else {
            bytes
        };
        let wanted = (2 - self.markup.len()).min(bytes.len());
        self.markup.extend_from_slice(&bytes[..wanted]);
    }

    fn has_bom(&self) -> bool {
        self.has_bom == Some(true)
    }

    /// Whether the markup starts like UTF-8 XML: a `<` not followed by NUL, as
    /// it would be in UTF-16.
    fn looks_like_utf8_xml(&self) -> bool {
        self.markup.first() == Some(&b'<') && self.markup.get(1).is_some_and(|byte| *byte != 0)
    }
}

/// Returns the byte range of the part to keep when it needs repair. The part's
/// size and CRC must match its central directory record.
fn inspect(
    entry: &Entry<'_>,
    budget: &mut u64,
    deadline: &Deadline,
) -> Result<Option<Range<u64>>, Unsupported> {
    let mut reader = entry.reader()?;
    let mut crc = Crc::new();
    let mut buffer = vec![0; BUFFER_LEN];
    let mut lead = Lead::default();
    let mut length = 0u64;
    // End of the last byte that is neither NUL nor XML whitespace.
    let mut content_end = 0u64;
    let mut padding_has_nul = false;
    loop {
        deadline.check()?;
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        crc.update(chunk);
        lead.feed(chunk);
        match chunk.iter().rposition(|byte| !is_padding(*byte)) {
            Some(index) => {
                content_end = length + index as u64 + 1;
                padding_has_nul = chunk[index + 1..].contains(&0);
            }
            None => padding_has_nul |= chunk.contains(&0),
        }
        length += read as u64;
        if length > u64::from(entry.uncompressed_size) || length > *budget {
            return Err(Unsupported);
        }
    }
    if length != u64::from(entry.uncompressed_size) || crc.sum() != entry.crc {
        return Err(Unsupported);
    }
    *budget -= length;

    lead.finish();
    let has_bom = lead.has_bom();
    let start = if has_bom { UTF8_BOM.len() } else { 0 };
    // Only a part that starts like UTF-8 XML, after any whitespace (kept), is
    // changed: dropping trailing NULs from a UTF-16 part would corrupt its last
    // character.
    if !(has_bom || padding_has_nul) || !lead.looks_like_utf8_xml() {
        return Ok(None);
    }
    let end = if padding_has_nul { content_end } else { length };
    Ok(Some(start as u64..end))
}

fn read_entries(package: &[u8]) -> Result<Vec<Entry<'_>>, Unsupported> {
    let last = package
        .len()
        .checked_sub(END_OF_CENTRAL_DIRECTORY_LEN)
        .ok_or(Unsupported)?;
    let first = last.saturating_sub(usize::from(u16::MAX));
    let end = (first..=last)
        .rev()
        .find(|offset| u32_at(package, *offset).ok() == Some(END_OF_CENTRAL_DIRECTORY))
        .ok_or(Unsupported)?;
    let disk = u16_at(package, end + 4)?;
    let directory_disk = u16_at(package, end + 6)?;
    let disk_entries = u16_at(package, end + 8)?;
    let total_entries = u16_at(package, end + 10)?;
    let directory_size = u32_at(package, end + 12)?;
    let directory_offset = u32_at(package, end + 16)?;
    let comment_len = usize::from(u16_at(package, end + 20)?);
    if disk != 0
        || directory_disk != 0
        || disk_entries != total_entries
        || total_entries == u16::MAX
        || directory_size == u32::MAX
        || directory_offset == u32::MAX
        || end + END_OF_CENTRAL_DIRECTORY_LEN + comment_len != package.len()
    {
        return Err(Unsupported);
    }
    let directory_start = usize::try_from(directory_offset)?;
    let directory_end = directory_start
        .checked_add(usize::try_from(directory_size)?)
        .filter(|directory_end| *directory_end <= end)
        .ok_or(Unsupported)?;

    let mut entries = Vec::with_capacity(usize::from(total_entries));
    let mut offset = directory_start;
    let mut compressed_total = 0usize;
    for _ in 0..total_entries {
        if u32_at(package, offset)? != CENTRAL_HEADER {
            return Err(Unsupported);
        }
        let flags = u16_at(package, offset + 8)?;
        let method = u16_at(package, offset + 10)?;
        let compressed_size = u32_at(package, offset + 20)?;
        let uncompressed_size = u32_at(package, offset + 24)?;
        let name_len = usize::from(u16_at(package, offset + 28)?);
        let extra_len = usize::from(u16_at(package, offset + 30)?);
        let entry_comment_len = usize::from(u16_at(package, offset + 32)?);
        let start_disk = u16_at(package, offset + 34)?;
        let local_offset = u32_at(package, offset + 42)?;
        if flags & ENCRYPTED != 0
            || ![STORED, DEFLATED].contains(&method)
            || start_disk != 0
            || [compressed_size, uncompressed_size, local_offset].contains(&u32::MAX)
            || (method == STORED && compressed_size != uncompressed_size)
        {
            return Err(Unsupported);
        }
        let name_start = offset + CENTRAL_HEADER_LEN;
        let name = bytes_at(package, name_start, name_len)?;
        let next = name_start + name_len + extra_len + entry_comment_len;
        if next > directory_end {
            return Err(Unsupported);
        }

        let local = usize::try_from(local_offset)?;
        if u32_at(package, local)? != LOCAL_HEADER {
            return Err(Unsupported);
        }
        let data_start = local
            + LOCAL_HEADER_LEN
            + usize::from(u16_at(package, local + 26)?)
            + usize::from(u16_at(package, local + 28)?);
        let data = bytes_at(package, data_start, usize::try_from(compressed_size)?)?;
        // Overlapping entries could otherwise multiply the copied bytes.
        compressed_total += data.len();
        if compressed_total > package.len() {
            return Err(Unsupported);
        }
        entries.push(Entry {
            version_made_by: u16_at(package, offset + 4)?,
            version_needed: u16_at(package, offset + 6)?,
            flags,
            method,
            time: u16_at(package, offset + 12)?,
            date: u16_at(package, offset + 14)?,
            crc: u32_at(package, offset + 16)?,
            uncompressed_size,
            internal_attributes: u16_at(package, offset + 36)?,
            external_attributes: u32_at(package, offset + 38)?,
            name,
            data,
        });
        offset = next;
    }
    if offset != directory_end {
        return Err(Unsupported);
    }
    Ok(entries)
}

/// Writes the entries in their original order, failing once the package would
/// exceed `max_len` bytes or the deadline passes. Unchanged parts keep their
/// compressed bytes; repaired parts are deflated again.
fn write_package(
    max_len: usize,
    entries: &[Entry<'_>],
    repairs: &[Option<Range<u64>>],
    deadline: &Deadline,
) -> Result<Vec<u8>, Unsupported> {
    let mut output = Vec::new();
    let mut directory = Vec::new();
    for (entry, repair) in entries.iter().zip(repairs) {
        deadline.check()?;
        let local_offset = u32::try_from(output.len())?;
        let (version_needed, flags, method, crc, data, uncompressed_size) = match repair {
            None => (
                entry.version_needed,
                entry.flags & !DATA_DESCRIPTOR,
                entry.method,
                entry.crc,
                Cow::Borrowed(entry.data),
                entry.uncompressed_size,
            ),
            Some(range) => {
                let mut reader = entry.reader()?;
                io::copy(&mut (&mut reader).take(range.start), &mut io::sink())?;
                let mut kept = reader.take(range.end - range.start);
                // The fastest level bounds the CPU a crafted part can cost.
                let mut encoder = DeflateEncoder::new(Vec::new(), Compression::fast());
                let mut crc = Crc::new();
                let mut buffer = vec![0; BUFFER_LEN];
                loop {
                    deadline.check()?;
                    let read = kept.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    crc.update(&buffer[..read]);
                    encoder.write_all(&buffer[..read])?;
                    if output.len() + directory.len() + encoder.get_ref().len() > max_len {
                        return Err(Unsupported);
                    }
                }
                (
                    entry.version_needed.max(VERSION_NEEDED),
                    entry.flags & UTF8_NAMES,
                    DEFLATED,
                    crc.sum(),
                    Cow::Owned(encoder.finish()?),
                    u32::try_from(range.end - range.start)?,
                )
            }
        };
        let compressed_size = u32::try_from(data.len())?;
        let name_len = u16::try_from(entry.name.len())?;

        put_u32(&mut output, LOCAL_HEADER);
        put_u16(&mut output, version_needed);
        put_u16(&mut output, flags);
        put_u16(&mut output, method);
        put_u16(&mut output, entry.time);
        put_u16(&mut output, entry.date);
        put_u32(&mut output, crc);
        put_u32(&mut output, compressed_size);
        put_u32(&mut output, uncompressed_size);
        put_u16(&mut output, name_len);
        put_u16(&mut output, 0);
        output.extend_from_slice(entry.name);
        output.extend_from_slice(&data);

        put_u32(&mut directory, CENTRAL_HEADER);
        put_u16(&mut directory, entry.version_made_by);
        put_u16(&mut directory, version_needed);
        put_u16(&mut directory, flags);
        put_u16(&mut directory, method);
        put_u16(&mut directory, entry.time);
        put_u16(&mut directory, entry.date);
        put_u32(&mut directory, crc);
        put_u32(&mut directory, compressed_size);
        put_u32(&mut directory, uncompressed_size);
        put_u16(&mut directory, name_len);
        put_u16(&mut directory, 0);
        put_u16(&mut directory, 0);
        put_u16(&mut directory, 0);
        put_u16(&mut directory, entry.internal_attributes);
        put_u32(&mut directory, entry.external_attributes);
        put_u32(&mut directory, local_offset);
        directory.extend_from_slice(entry.name);
        if output.len() + directory.len() > max_len {
            return Err(Unsupported);
        }
    }
    let directory_offset = u32::try_from(output.len())?;
    let directory_size = u32::try_from(directory.len())?;
    let entry_count = u16::try_from(entries.len())?;
    output.extend_from_slice(&directory);
    put_u32(&mut output, END_OF_CENTRAL_DIRECTORY);
    put_u16(&mut output, 0);
    put_u16(&mut output, 0);
    put_u16(&mut output, entry_count);
    put_u16(&mut output, entry_count);
    put_u32(&mut output, directory_size);
    put_u32(&mut output, directory_offset);
    put_u16(&mut output, 0);
    if output.len() > max_len {
        return Err(Unsupported);
    }
    Ok(output)
}

fn bytes_at(bytes: &[u8], offset: usize, len: usize) -> Result<&[u8], Unsupported> {
    bytes
        .get(offset..offset.checked_add(len).ok_or(Unsupported)?)
        .ok_or(Unsupported)
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, Unsupported> {
    let value = bytes_at(bytes, offset, 2)?
        .try_into()
        .map_err(|_| Unsupported)?;
    Ok(u16::from_le_bytes(value))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, Unsupported> {
    let value = bytes_at(bytes, offset, 4)?
        .try_into()
        .map_err(|_| Unsupported)?;
    Ok(u32::from_le_bytes(value))
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

/// Builds and reads packages for tests.
#[cfg(test)]
pub(super) mod test_package {
    use super::*;

    /// A package of `(name, contents)` parts; the even-numbered ones deflated.
    pub fn build(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let compressed: Vec<(u16, Vec<u8>)> = parts
            .iter()
            .enumerate()
            .map(|(index, (_, contents))| {
                if index % 2 == 0 {
                    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
                    encoder.write_all(contents).unwrap();
                    (DEFLATED, encoder.finish().unwrap())
                } else {
                    (STORED, contents.to_vec())
                }
            })
            .collect();
        let entries: Vec<Entry<'_>> = parts
            .iter()
            .zip(&compressed)
            .map(|((name, contents), (method, data))| {
                let mut crc = Crc::new();
                crc.update(contents);
                Entry {
                    version_made_by: 20,
                    version_needed: if *method == DEFLATED { 20 } else { 10 },
                    flags: 0,
                    method: *method,
                    time: 0,
                    date: 0x21,
                    crc: crc.sum(),
                    uncompressed_size: contents.len() as u32,
                    internal_attributes: 0,
                    external_attributes: 0,
                    name: name.as_bytes(),
                    data,
                }
            })
            .collect();
        write_package(usize::MAX, &entries, &vec![None; entries.len()], &later()).unwrap()
    }

    /// A deadline a minute away.
    pub fn later() -> Deadline {
        Deadline::new(
            Instant::now() + std::time::Duration::from_secs(60),
            Arc::default(),
        )
    }

    /// The package's parts, with sizes and CRCs checked.
    pub fn parts(package: &[u8]) -> Vec<(String, Vec<u8>)> {
        read_entries(package)
            .unwrap()
            .iter()
            .map(|entry| {
                let mut contents = Vec::new();
                entry.reader().unwrap().read_to_end(&mut contents).unwrap();
                let mut crc = Crc::new();
                crc.update(&contents);
                assert_eq!(contents.len() as u64, u64::from(entry.uncompressed_size));
                assert_eq!(crc.sum(), entry.crc);
                (String::from_utf8(entry.name.to_vec()).unwrap(), contents)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{test_package::*, *};

    fn fix(package: &[u8]) -> Option<(Vec<u8>, usize)> {
        repair(package, &later())
    }

    #[test]
    fn strips_byte_order_marks_and_nul_padding_from_xml_parts() {
        let package = build(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("_rels/.rels", b"\xEF\xBB\xBF<Relationships/>"),
            ("xl/workbook.xml", b"<workbook/>\n\0\0\0\0"),
            ("xl/styles.xml", b"\xEF\xBB\xBF<styleSheet/>\r\n\0 \0"),
            ("xl/worksheets/sheet1.xml", b"<worksheet/>\r\n"),
            ("docProps/thumbnail.jpeg", b"\xFF\xD8\xFF\0\0"),
            ("EPUB/content.opf", b"<package/>\0\0"),
        ]);
        let (repaired, count) = fix(&package).expect("parts need repair");
        assert_eq!(count, 4);
        assert_eq!(
            parts(&repaired),
            [
                ("[Content_Types].xml", &b"<Types/>"[..]),
                ("_rels/.rels", b"<Relationships/>"),
                ("xl/workbook.xml", b"<workbook/>"),
                ("xl/styles.xml", b"<styleSheet/>"),
                ("xl/worksheets/sheet1.xml", b"<worksheet/>\r\n"),
                ("docProps/thumbnail.jpeg", b"\xFF\xD8\xFF\0\0"),
                ("EPUB/content.opf", b"<package/>"),
            ]
            .map(|(name, contents)| (name.to_owned(), contents.to_vec()))
        );
        assert!(fix(&repaired).is_none(), "a repaired package is stable");
    }

    #[test]
    fn repaired_packages_cannot_outgrow_their_bound() {
        let package = build(&[("xl/workbook.xml", b"\xEF\xBB\xBF<workbook/>")]);
        let entries = read_entries(&package).unwrap();
        let mut budget = MAX_INFLATED_BYTES;
        let repairs = [inspect(&entries[0], &mut budget, &later()).unwrap()];
        assert!(repairs[0].is_some());
        let repaired = write_package(usize::MAX, &entries, &repairs, &later()).unwrap();
        assert!(write_package(repaired.len(), &entries, &repairs, &later()).is_ok());
        assert!(write_package(repaired.len() - 1, &entries, &repairs, &later()).is_err());
    }

    #[test]
    fn keeps_whitespace_before_the_root_and_strips_padding_after_it() {
        let package = build(&[
            (
                "word/document.xml",
                b"\r\n  \t<?xml version=\"1.0\"?><document/>\0\0\0",
            ),
            ("word/styles.xml", b"\xEF\xBB\xBF\n <styles/>\n\0"),
            ("word/settings.xml", b"\n\n<settings/>\n"),
        ]);
        let (repaired, count) = fix(&package).expect("parts need repair");
        assert_eq!(count, 2);
        assert_eq!(
            parts(&repaired),
            [
                (
                    "word/document.xml",
                    &b"\r\n  \t<?xml version=\"1.0\"?><document/>"[..]
                ),
                ("word/styles.xml", b"\n <styles/>"),
                ("word/settings.xml", b"\n\n<settings/>\n"),
            ]
            .map(|(name, contents)| (name.to_owned(), contents.to_vec()))
        );
        // Any amount of whitespace, across read chunks, is kept.
        for spaces in [8_192, BUFFER_LEN + 7] {
            let contents = [&vec![b' '; spaces][..], b"<document/>\0"].concat();
            let (repaired, _) = fix(&build(&[("word/document.xml", &contents)]))
                .unwrap_or_else(|| panic!("{spaces} spaces"));
            assert_eq!(parts(&repaired)[0].1, contents[..contents.len() - 1]);
        }
        // Whitespace before a UTF-16 part's root is still not UTF-8 XML.
        assert!(fix(&build(&[("word/document.xml", b" \0<\0a\0/\0>\0")])).is_none());
        // Nor is a part that is only whitespace.
        assert!(fix(&build(&[("word/document.xml", b"  \n\0")])).is_none());
    }

    #[test]
    fn packages_with_other_compression_methods_are_left_unchanged() {
        let parts = [
            ("xl/workbook.xml", &b"<workbook/>\0"[..]),
            ("xl/media/image1.bin", b"BZh91AY&SY"),
        ];
        assert!(
            fix(&build(&parts)).is_some(),
            "repairable with stored parts"
        );
        // Mark the stored binary part as bzip2 (method 12), in its central
        // directory record and in its local header.
        let mut mixed = build(&parts);
        let directory = u32_at(&mixed, mixed.len() - 6).unwrap() as usize;
        let second = directory + CENTRAL_HEADER_LEN + parts[0].0.len();
        assert_eq!(u32_at(&mixed, second).unwrap(), CENTRAL_HEADER);
        let local = u32_at(&mixed, second + 42).unwrap() as usize;
        for method in [second + 10, local + 8] {
            mixed[method..method + 2].copy_from_slice(&12u16.to_le_bytes());
        }
        assert!(fix(&mixed).is_none());
    }

    #[test]
    fn repairs_stop_at_the_deadline_or_when_cancelled() {
        let package = build(&[("xl/workbook.xml", b"<workbook/>\0")]);
        let expired = Deadline::new(Instant::now(), Arc::default());
        assert!(repair(&package, &expired).is_none());
        let cancelled = later();
        cancelled.cancelled.store(true, Ordering::Relaxed);
        assert!(repair(&package, &cancelled).is_none());
        assert!(repair(&package, &later()).is_some());
    }

    #[test]
    fn leaves_sound_and_unreadable_packages_to_pandoc() {
        let sound = build(&[("xl/workbook.xml", b"<workbook/>\n")]);
        assert!(fix(&sound).is_none());
        // UTF-16 parts end in NUL bytes that are part of their last character.
        let utf16 = build(&[("xl/workbook.xml", b"<\0w\0/\0>\0")]);
        assert!(fix(&utf16).is_none());
        // Non-XML parts are never inspected.
        assert!(fix(&build(&[("xl/media/image1.png", b"\xEF\xBB\xBF<\0")])).is_none());

        let padded = build(&[("xl/workbook.xml", b"<workbook/>\0")]);
        assert!(fix(&padded).is_some());
        assert!(fix(b"PK\x03\x04 not a complete archive").is_none());
        assert!(fix(&padded[..padded.len() - 1]).is_none(), "truncated");
        let mut trailing = padded.clone();
        trailing.push(0);
        assert!(fix(&trailing).is_none(), "data after the archive");

        // The central directory's offset, from the end record.
        let directory = u32_at(&padded, padded.len() - 6).unwrap() as usize;
        let mut encrypted = padded.clone();
        encrypted[directory + 8] |= 1;
        assert!(fix(&encrypted).is_none(), "encrypted");
        let mut understated = padded.clone();
        understated[directory + 24] -= 1;
        assert!(fix(&understated).is_none(), "larger than its recorded size");
        let mut corrupt = padded.clone();
        corrupt[directory + 16] ^= 1;
        assert!(fix(&corrupt).is_none(), "CRC mismatch");
        let mut compressed = padded;
        compressed[directory + 10] = 14;
        assert!(fix(&compressed).is_none(), "unsupported compression method");
    }
}
