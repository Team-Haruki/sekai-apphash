//! Reads the identity attributes of a compiled (binary XML) AndroidManifest.

use serde::Serialize;

use crate::error::{Error, Result};

const CHUNK_XML: u16 = 0x0003;
const CHUNK_STRING_POOL: u16 = 0x0001;
const CHUNK_RESOURCE_MAP: u16 = 0x0180;
const CHUNK_START_ELEMENT: u16 = 0x0102;
const UTF8_FLAG: u32 = 0x100;
const NO_INDEX: u32 = u32::MAX;
const TYPE_STRING: u8 = 0x03;
const TYPE_INT_DEC: u8 = 0x10;
const TYPE_INT_HEX: u8 = 0x11;
const ATTR_VERSION_CODE: u32 = 0x0101_021b;
const ATTR_VERSION_NAME: u32 = 0x0101_021c;

/// The `<manifest>` attributes that identify a build.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestInfo {
    pub package: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_code: Option<u32>,
}

/// Parses the root `<manifest>` element of a binary AndroidManifest.xml.
pub fn parse_manifest(data: &[u8]) -> Result<ManifestInfo> {
    if data.len() < 8 || u16_at(data, 0)? != CHUNK_XML {
        return Err(Error::Manifest("not a binary XML document".into()));
    }
    let mut pos = usize::from(u16_at(data, 2)?);
    let mut strings: Vec<String> = Vec::new();
    let mut resource_ids: Vec<u32> = Vec::new();
    while pos + 8 <= data.len() {
        let kind = u16_at(data, pos)?;
        let header_size = usize::from(u16_at(data, pos + 2)?);
        let size = u32_at(data, pos + 4)? as usize;
        if size < 8 || header_size > size || pos + size > data.len() {
            return Err(Error::Manifest(format!("malformed chunk at {pos}")));
        }
        let chunk = &data[pos..pos + size];
        match kind {
            CHUNK_STRING_POOL => strings = parse_string_pool(chunk, header_size)?,
            CHUNK_RESOURCE_MAP => {
                resource_ids = chunk[header_size..]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|id| u32::from_le_bytes(*id))
                    .collect();
            }
            CHUNK_START_ELEMENT => {
                let name = string(&strings, u32_at(chunk, 20)?);
                if name != Some("manifest") {
                    return Err(Error::Manifest(format!(
                        "root element is {name:?}, expected manifest"
                    )));
                }
                return read_manifest_element(chunk, header_size, &strings, &resource_ids);
            }
            _ => {}
        }
        pos += size;
    }
    Err(Error::Manifest("no <manifest> element".into()))
}

fn read_manifest_element(
    chunk: &[u8],
    header_size: usize,
    strings: &[String],
    resource_ids: &[u32],
) -> Result<ManifestInfo> {
    let ext = header_size;
    let attribute_start = usize::from(u16_at(chunk, ext + 8)?);
    let attribute_size = usize::from(u16_at(chunk, ext + 10)?);
    let attribute_count = usize::from(u16_at(chunk, ext + 12)?);
    if attribute_size < 20 {
        return Err(Error::Manifest("attribute records are too small".into()));
    }
    let mut info = ManifestInfo::default();
    for index in 0..attribute_count {
        let at = ext + attribute_start + index * attribute_size;
        let name_index = u32_at(chunk, at + 4)?;
        let raw_value = u32_at(chunk, at + 8)?;
        let data_type = *chunk
            .get(at + 15)
            .ok_or_else(|| Error::Manifest("truncated attribute".into()))?;
        let data = u32_at(chunk, at + 16)?;
        // Obfuscated manifests blank attribute names; the resource map still
        // carries the framework attribute id at the same index.
        let resource_id = resource_ids.get(name_index as usize).copied();
        let name = string(strings, name_index).unwrap_or_default();
        let text = || {
            string(strings, raw_value)
                .or_else(|| {
                    (data_type == TYPE_STRING)
                        .then(|| string(strings, data))
                        .flatten()
                })
                .map(str::to_owned)
        };
        if name == "package" {
            info.package = text().unwrap_or_default();
        } else if name == "versionName" || resource_id == Some(ATTR_VERSION_NAME) {
            info.version_name = text();
        } else if name == "versionCode" || resource_id == Some(ATTR_VERSION_CODE) {
            info.version_code = match data_type {
                TYPE_INT_DEC | TYPE_INT_HEX => Some(data),
                _ => text().and_then(|value| value.parse().ok()),
            };
        }
    }
    if info.package.is_empty() {
        return Err(Error::Manifest(
            "<manifest> has no package attribute".into(),
        ));
    }
    Ok(info)
}

fn parse_string_pool(chunk: &[u8], header_size: usize) -> Result<Vec<String>> {
    let count = u32_at(chunk, 8)? as usize;
    let flags = u32_at(chunk, 16)?;
    let strings_start = u32_at(chunk, 20)? as usize;
    if header_size + count.saturating_mul(4) > chunk.len() || strings_start > chunk.len() {
        return Err(Error::Manifest("string pool offsets out of range".into()));
    }
    let utf8 = flags & UTF8_FLAG != 0;
    (0..count)
        .map(|index| {
            let offset = u32_at(chunk, header_size + index * 4)? as usize;
            let at = strings_start + offset;
            let bytes = chunk
                .get(at..)
                .ok_or_else(|| Error::Manifest("string offset out of range".into()))?;
            if utf8 {
                read_utf8(bytes)
            } else {
                read_utf16(bytes)
            }
        })
        .collect()
}

fn read_utf8(bytes: &[u8]) -> Result<String> {
    let (_chars, used) = utf8_length(bytes)?;
    let (len, used2) = utf8_length(&bytes[used..])?;
    let start = used + used2;
    let text = bytes
        .get(start..start + len)
        .ok_or_else(|| Error::Manifest("UTF-8 string overruns the pool".into()))?;
    Ok(String::from_utf8_lossy(text).into_owned())
}

fn utf8_length(bytes: &[u8]) -> Result<(usize, usize)> {
    let first = *bytes
        .first()
        .ok_or_else(|| Error::Manifest("truncated string length".into()))?;
    if first & 0x80 == 0 {
        return Ok((usize::from(first), 1));
    }
    let second = *bytes
        .get(1)
        .ok_or_else(|| Error::Manifest("truncated string length".into()))?;
    Ok(((usize::from(first & 0x7F) << 8) | usize::from(second), 2))
}

fn read_utf16(bytes: &[u8]) -> Result<String> {
    let first = usize::from(u16_at(bytes, 0)?);
    let (len, start) = if first & 0x8000 == 0 {
        (first, 2)
    } else {
        (((first & 0x7FFF) << 16) | usize::from(u16_at(bytes, 2)?), 4)
    };
    let units = bytes
        .get(start..start + len * 2)
        .ok_or_else(|| Error::Manifest("UTF-16 string overruns the pool".into()))?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|unit| u16::from_le_bytes(*unit))
        .collect::<Vec<_>>();
    Ok(String::from_utf16_lossy(&units))
}

fn string(strings: &[String], index: u32) -> Option<&str> {
    if index == NO_INDEX {
        return None;
    }
    strings
        .get(index as usize)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

fn u16_at(data: &[u8], at: usize) -> Result<u16> {
    data.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| Error::Manifest(format!("truncated at {at}")))
}

fn u32_at(data: &[u8], at: usize) -> Result<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| Error::Manifest(format!("truncated at {at}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{manifest, Attr, AttrValue};

    fn sample(utf8: bool, obfuscated: bool) -> Vec<u8> {
        let name = |plain| if obfuscated { "" } else { plain };
        manifest(
            &[
                Attr {
                    name: name("versionCode"),
                    resource_id: Some(ATTR_VERSION_CODE),
                    value: AttrValue::Int(300_129),
                },
                Attr {
                    name: name("versionName"),
                    resource_id: Some(ATTR_VERSION_NAME),
                    value: AttrValue::Text("7.0.0"),
                },
                Attr {
                    name: "package",
                    resource_id: None,
                    value: AttrValue::Text("com.sega.pjsekai"),
                },
            ],
            utf8,
        )
    }

    #[test]
    fn reads_package_and_versions() {
        for utf8 in [true, false] {
            for obfuscated in [false, true] {
                let info = parse_manifest(&sample(utf8, obfuscated)).unwrap();
                assert_eq!(
                    info,
                    ManifestInfo {
                        package: "com.sega.pjsekai".into(),
                        version_name: Some("7.0.0".into()),
                        version_code: Some(300_129),
                    },
                    "utf8={utf8} obfuscated={obfuscated}"
                );
            }
        }
    }

    #[test]
    fn rejects_non_manifests() {
        assert!(parse_manifest(b"<?xml version").is_err());
        assert!(parse_manifest(&[]).is_err());
        let mut doc = sample(true, false);
        doc.truncate(doc.len() - 30);
        assert!(parse_manifest(&doc).is_err());
        let only_package = manifest(
            &[Attr {
                name: "versionName",
                resource_id: None,
                value: AttrValue::Text("1.0"),
            }],
            true,
        );
        assert!(parse_manifest(&only_package).is_err());
    }
}
