//! A minimal read-only zip reader over a random-access [`Region`].
//!
//! Only the central directory and the entries a caller asks for are read, so
//! on a remote [`Region`] an APK, XAPK or IPA costs a few range requests
//! rather than a full download. Stored entries are exposed as subregions
//! without copying, which also lets a stored APK inside an XAPK be opened as
//! a nested archive.

use std::io::Read;

use flate2::read::DeflateDecoder;
use unity_rs_core::source::Region;

use crate::error::{Error, Result};

const EOCD_SIGNATURE: u32 = 0x0605_4b50;
const ZIP64_LOCATOR_SIGNATURE: u32 = 0x0706_4b50;
const ZIP64_EOCD_SIGNATURE: u32 = 0x0606_4b50;
const CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const EOCD_MIN_LEN: u64 = 22;
const MAX_COMMENT_LEN: u64 = 0xFFFF;
const MAX_CENTRAL_DIRECTORY: u64 = 64 * 1024 * 1024;

pub const METHOD_STORED: u16 = 0;
pub const METHOD_DEFLATED: u16 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    pub name: String,
    pub method: u16,
    pub flags: u16,
    pub crc32: u32,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
}

impl ZipEntry {
    /// The last path component, accepting both separators.
    #[must_use]
    pub fn base_name(&self) -> &str {
        self.name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(self.name.as_str())
    }

    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        self.flags & 1 != 0
    }
}

#[derive(Debug, Clone)]
pub struct ZipArchive {
    region: Region,
    entries: Vec<ZipEntry>,
}

impl ZipArchive {
    pub fn open(region: Region) -> Result<Self> {
        let (cd_offset, cd_size, cd_count) = locate_central_directory(&region)?;
        if cd_size > MAX_CENTRAL_DIRECTORY {
            return Err(Error::Zip(format!(
                "central directory of {cd_size} bytes exceeds the {MAX_CENTRAL_DIRECTORY} byte limit"
            )));
        }
        let end = cd_offset
            .checked_add(cd_size)
            .filter(|end| *end <= region.len())
            .ok_or_else(|| Error::Zip("central directory lies outside the archive".into()))?;
        let mut directory = vec![0u8; (end - cd_offset) as usize];
        region.read_exact_at(cd_offset, &mut directory)?;
        let entries = parse_central_directory(&directory, cd_count)?;
        Ok(Self { region, entries })
    }

    #[must_use]
    pub fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    #[must_use]
    pub const fn region(&self) -> &Region {
        &self.region
    }

    pub fn by_name(&self, name: &str) -> Option<&ZipEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// Offset of the entry's (possibly compressed) data within the archive.
    pub fn data_offset(&self, entry: &ZipEntry) -> Result<u64> {
        let mut header = [0u8; 30];
        self.region
            .read_exact_at(entry.local_header_offset, &mut header)
            .map_err(|error| Error::Zip(format!("{}: local header: {error}", entry.name)))?;
        if le_u32(&header, 0) != LOCAL_SIGNATURE {
            return Err(Error::Zip(format!(
                "{}: bad local header signature",
                entry.name
            )));
        }
        let name_len = u64::from(le_u16(&header, 26));
        let extra_len = u64::from(le_u16(&header, 28));
        let start = entry.local_header_offset + 30 + name_len + extra_len;
        if start
            .checked_add(entry.compressed_size)
            .is_none_or(|end| end > self.region.len())
        {
            return Err(Error::Zip(format!(
                "{}: data lies outside the archive",
                entry.name
            )));
        }
        Ok(start)
    }

    /// The bytes of a stored entry as a subregion, without reading them.
    pub fn stored_region(&self, entry: &ZipEntry) -> Result<Region> {
        if entry.is_encrypted() {
            return Err(Error::Zip(format!("{} is encrypted", entry.name)));
        }
        if entry.method != METHOD_STORED || entry.compressed_size != entry.uncompressed_size {
            return Err(Error::Zip(format!(
                "{} is not stored (method {})",
                entry.name, entry.method
            )));
        }
        let start = self.data_offset(entry)?;
        Ok(self.region.subregion(start, entry.compressed_size)?)
    }

    /// Reads and, if needed, inflates one entry into memory.
    pub fn read(&self, entry: &ZipEntry, max_bytes: u64) -> Result<Vec<u8>> {
        if entry.uncompressed_size > max_bytes {
            return Err(Error::Zip(format!(
                "{} is {} bytes, above the {max_bytes} byte limit",
                entry.name, entry.uncompressed_size
            )));
        }
        if entry.is_encrypted() {
            return Err(Error::Zip(format!("{} is encrypted", entry.name)));
        }
        let start = self.data_offset(entry)?;
        let raw = self.region.subregion(start, entry.compressed_size)?;
        match entry.method {
            METHOD_STORED => Ok(raw.read_to_vec(max_bytes)?),
            METHOD_DEFLATED => {
                let mut output = Vec::with_capacity(entry.uncompressed_size as usize);
                DeflateDecoder::new(raw.cursor())
                    .take(max_bytes + 1)
                    .read_to_end(&mut output)
                    .map_err(|error| Error::Zip(format!("{}: inflate: {error}", entry.name)))?;
                if output.len() as u64 != entry.uncompressed_size {
                    return Err(Error::Zip(format!(
                        "{}: inflated {} bytes, expected {}",
                        entry.name,
                        output.len(),
                        entry.uncompressed_size
                    )));
                }
                Ok(output)
            }
            method => Err(Error::Zip(format!(
                "{}: unsupported compression method {method}",
                entry.name
            ))),
        }
    }

    /// A random-access region holding the entry's uncompressed bytes: a
    /// zero-copy subregion for stored entries, an in-memory copy otherwise.
    pub fn entry_region(&self, entry: &ZipEntry, max_inflate_bytes: u64) -> Result<Region> {
        if entry.method == METHOD_STORED {
            return self.stored_region(entry);
        }
        Ok(Region::from_bytes(self.read(entry, max_inflate_bytes)?))
    }
}

fn locate_central_directory(region: &Region) -> Result<(u64, u64, u64)> {
    let len = region.len();
    if len < EOCD_MIN_LEN {
        return Err(Error::Zip("too short to be a zip archive".into()));
    }
    let tail_len = len.min(EOCD_MIN_LEN + MAX_COMMENT_LEN);
    let tail_start = len - tail_len;
    let mut tail = vec![0u8; tail_len as usize];
    region.read_exact_at(tail_start, &mut tail)?;
    let eocd = (0..=tail.len() - EOCD_MIN_LEN as usize)
        .rev()
        .find(|&pos| le_u32(&tail, pos) == EOCD_SIGNATURE)
        .ok_or_else(|| Error::Zip("end of central directory not found".into()))?;
    let count = u64::from(le_u16(&tail, eocd + 10));
    let size = u64::from(le_u32(&tail, eocd + 12));
    let offset = u64::from(le_u32(&tail, eocd + 16));
    if count != 0xFFFF && size != 0xFFFF_FFFF && offset != 0xFFFF_FFFF {
        return Ok((offset, size, count));
    }

    // Zip64: the locator sits immediately before the classic record.
    let eocd_abs = tail_start + eocd as u64;
    let locator_abs = eocd_abs
        .checked_sub(20)
        .ok_or_else(|| Error::Zip("zip64 locator missing".into()))?;
    let mut locator = [0u8; 20];
    region.read_exact_at(locator_abs, &mut locator)?;
    if le_u32(&locator, 0) != ZIP64_LOCATOR_SIGNATURE {
        return Err(Error::Zip("zip64 locator missing".into()));
    }
    let record_offset = le_u64(&locator, 8);
    let mut record = [0u8; 56];
    if record_offset.checked_add(56).is_none_or(|end| end > len) {
        return Err(Error::Zip(
            "zip64 end record lies outside the archive".into(),
        ));
    }
    region.read_exact_at(record_offset, &mut record)?;
    if le_u32(&record, 0) != ZIP64_EOCD_SIGNATURE {
        return Err(Error::Zip("zip64 end record signature mismatch".into()));
    }
    Ok((
        le_u64(&record, 48),
        le_u64(&record, 40),
        le_u64(&record, 32),
    ))
}

fn parse_central_directory(data: &[u8], expected: u64) -> Result<Vec<ZipEntry>> {
    let mut entries = Vec::with_capacity(expected.min(1 << 16) as usize);
    let mut pos = 0usize;
    while pos + 46 <= data.len() && le_u32(data, pos) == CENTRAL_SIGNATURE {
        let flags = le_u16(data, pos + 8);
        let method = le_u16(data, pos + 10);
        let crc32 = le_u32(data, pos + 16);
        let mut compressed_size = u64::from(le_u32(data, pos + 20));
        let mut uncompressed_size = u64::from(le_u32(data, pos + 24));
        let name_len = usize::from(le_u16(data, pos + 28));
        let extra_len = usize::from(le_u16(data, pos + 30));
        let comment_len = usize::from(le_u16(data, pos + 32));
        let mut local_header_offset = u64::from(le_u32(data, pos + 42));
        let name_end = pos + 46 + name_len;
        let extra_end = name_end + extra_len;
        let next = extra_end + comment_len;
        if next > data.len() {
            return Err(Error::Zip("truncated central directory entry".into()));
        }
        let name = String::from_utf8_lossy(&data[pos + 46..name_end]).into_owned();
        apply_zip64_extra(
            &data[name_end..extra_end],
            &mut uncompressed_size,
            &mut compressed_size,
            &mut local_header_offset,
        );
        entries.push(ZipEntry {
            name,
            method,
            flags,
            crc32,
            compressed_size,
            uncompressed_size,
            local_header_offset,
        });
        pos = next;
    }
    if entries.is_empty() {
        return Err(Error::Zip("empty central directory".into()));
    }
    if expected != 0xFFFF && entries.len() as u64 != expected {
        return Err(Error::Zip(format!(
            "central directory lists {} entries, end record says {expected}",
            entries.len()
        )));
    }
    Ok(entries)
}

fn apply_zip64_extra(extra: &[u8], uncompressed: &mut u64, compressed: &mut u64, offset: &mut u64) {
    let mut pos = 0usize;
    while pos + 4 <= extra.len() {
        let id = le_u16(extra, pos);
        let size = usize::from(le_u16(extra, pos + 2));
        let body_end = pos + 4 + size;
        if body_end > extra.len() {
            return;
        }
        if id == 0x0001 {
            let body = &extra[pos + 4..body_end];
            let mut cursor = 0usize;
            for field in [uncompressed, compressed, offset] {
                if *field == 0xFFFF_FFFF && cursor + 8 <= body.len() {
                    *field = le_u64(body, cursor);
                    cursor += 8;
                }
            }
            return;
        }
        pos = body_end;
    }
}

fn le_u16(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn le_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("4 bytes"))
}

fn le_u64(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

    use super::*;

    fn build(files: &[(&str, &[u8], CompressionMethod)], zip64: bool, comment: &str) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data, method) in files {
            let options = SimpleFileOptions::default()
                .compression_method(*method)
                .large_file(zip64);
            writer.start_file(*name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.set_comment(comment.to_owned()).unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn reads_stored_and_deflated_entries() {
        let text = b"hello hello hello hello hello".repeat(50);
        let bytes = build(
            &[
                ("a/stored.bin", b"stored-bytes", CompressionMethod::Stored),
                ("b/deflated.txt", &text, CompressionMethod::Deflated),
            ],
            false,
            "a trailing comment",
        );
        let archive = ZipArchive::open(Region::from_bytes(bytes)).unwrap();
        assert_eq!(archive.entries().len(), 2);
        let stored = archive.by_name("a/stored.bin").unwrap();
        assert_eq!(stored.base_name(), "stored.bin");
        assert_eq!(
            archive
                .stored_region(stored)
                .unwrap()
                .read_to_vec(100)
                .unwrap(),
            b"stored-bytes"
        );
        let deflated = archive.by_name("b/deflated.txt").unwrap();
        assert_eq!(deflated.method, METHOD_DEFLATED);
        assert!(archive.stored_region(deflated).is_err());
        assert_eq!(archive.read(deflated, 1 << 20).unwrap(), text);
        assert_eq!(
            archive
                .entry_region(deflated, 1 << 20)
                .unwrap()
                .read_to_vec(1 << 20)
                .unwrap(),
            text
        );
        assert!(archive.read(deflated, 10).is_err());
    }

    #[test]
    fn reads_zip64_archives() {
        let bytes = build(
            &[("x.bin", b"0123456789", CompressionMethod::Stored)],
            true,
            "",
        );
        let archive = ZipArchive::open(Region::from_bytes(bytes)).unwrap();
        let entry = archive.by_name("x.bin").unwrap();
        assert_eq!(archive.read(entry, 100).unwrap(), b"0123456789");
    }

    #[test]
    fn opens_a_stored_archive_nested_in_another() {
        let inner = build(
            &[(
                "assets/bin/Data/data.unity3d",
                b"UnityFS\0...",
                CompressionMethod::Stored,
            )],
            false,
            "",
        );
        let outer = build(
            &[
                ("manifest.json", b"{}", CompressionMethod::Deflated),
                ("UnityDataAssetPack.apk", &inner, CompressionMethod::Stored),
            ],
            false,
            "",
        );
        let archive = ZipArchive::open(Region::from_bytes(outer)).unwrap();
        let nested_entry = archive.by_name("UnityDataAssetPack.apk").unwrap();
        let nested = ZipArchive::open(archive.stored_region(nested_entry).unwrap()).unwrap();
        let data = nested.by_name("assets/bin/Data/data.unity3d").unwrap();
        assert_eq!(nested.read(data, 100).unwrap(), b"UnityFS\0...");
    }

    #[test]
    fn rejects_garbage() {
        assert!(ZipArchive::open(Region::from_bytes(vec![0u8; 10])).is_err());
        assert!(ZipArchive::open(Region::from_bytes(vec![0u8; 4096])).is_err());
        let mut bytes = build(&[("a", b"a", CompressionMethod::Stored)], false, "");
        // Corrupt the local header signature.
        bytes[0] = b'X';
        let archive = ZipArchive::open(Region::from_bytes(bytes)).unwrap();
        let entry = archive.by_name("a").unwrap().clone();
        assert!(archive.read(&entry, 10).is_err());
    }

    #[test]
    fn zip64_extra_overrides_only_saturated_fields() {
        let mut extra = vec![0x01, 0x00, 16, 0];
        extra.extend_from_slice(&5_000_000_000u64.to_le_bytes());
        extra.extend_from_slice(&7u64.to_le_bytes());
        let (mut u, mut c, mut o) = (0xFFFF_FFFF, 0xFFFF_FFFF, 12);
        apply_zip64_extra(&extra, &mut u, &mut c, &mut o);
        assert_eq!((u, c, o), (5_000_000_000, 7, 12));
    }
}
