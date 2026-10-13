//! Synthetic fixture builders shared by the unit and integration tests.
//!
//! Real game packages are copyrighted and large, so the tests build the same
//! structures from scratch: a binary AndroidManifest, `PlayerSettingConfig`
//! payloads, a Unity serialized file (format 22, no type trees, as player
//! builds ship), a LZ4-compressed UnityFS bundle, and zip archives (APK /
//! XAPK with a stored nested APK). Not part of the supported API.
#![doc(hidden)]
#![allow(clippy::cast_possible_truncation, missing_docs)]

use std::io::Write;

use flate2::write::DeflateEncoder;
use flate2::Compression;

use crate::player_settings::{Layout, PlayerSettings};

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

// ---------------------------------------------------------------- manifest --

pub struct Attr<'a> {
    pub name: &'a str,
    pub resource_id: Option<u32>,
    pub value: AttrValue<'a>,
}

pub enum AttrValue<'a> {
    Text(&'a str),
    Int(u32),
}

/// Encodes a binary-XML document whose root is `<manifest>` with `attrs`,
/// using a UTF-8 or UTF-16 string pool.
#[must_use]
pub fn manifest(attrs: &[Attr<'_>], utf8: bool) -> Vec<u8> {
    // Attribute names first (so the resource map lines up), then "manifest",
    // then the string values.
    let mut pool: Vec<String> = attrs.iter().map(|a| a.name.to_owned()).collect();
    let manifest_index = pool.len() as u32;
    pool.push("manifest".into());
    let mut value_index = Vec::new();
    for attr in attrs {
        if let AttrValue::Text(text) = attr.value {
            value_index.push(pool.len() as u32);
            pool.push(text.to_owned());
        } else {
            value_index.push(u32::MAX);
        }
    }

    let mut strings = Vec::new();
    let mut offsets = Vec::new();
    for s in &pool {
        offsets.push(strings.len() as u32);
        if utf8 {
            strings.push(s.chars().count() as u8);
            strings.push(s.len() as u8);
            strings.extend_from_slice(s.as_bytes());
            strings.push(0);
        } else {
            let units: Vec<u16> = s.encode_utf16().collect();
            push_u16(&mut strings, units.len() as u16);
            for unit in units {
                push_u16(&mut strings, unit);
            }
            push_u16(&mut strings, 0);
        }
    }
    while !strings.len().is_multiple_of(4) {
        strings.push(0);
    }
    let header = 28u32;
    let strings_start = header + 4 * pool.len() as u32;
    let mut pool_chunk = Vec::new();
    push_u16(&mut pool_chunk, 0x0001);
    push_u16(&mut pool_chunk, header as u16);
    push_u32(&mut pool_chunk, strings_start + strings.len() as u32);
    push_u32(&mut pool_chunk, pool.len() as u32);
    push_u32(&mut pool_chunk, 0);
    push_u32(&mut pool_chunk, if utf8 { 0x100 } else { 0 });
    push_u32(&mut pool_chunk, strings_start);
    push_u32(&mut pool_chunk, 0);
    for offset in offsets {
        push_u32(&mut pool_chunk, offset);
    }
    pool_chunk.extend_from_slice(&strings);

    let mut map_chunk = Vec::new();
    push_u16(&mut map_chunk, 0x0180);
    push_u16(&mut map_chunk, 8);
    push_u32(&mut map_chunk, 8 + 4 * attrs.len() as u32);
    for attr in attrs {
        push_u32(&mut map_chunk, attr.resource_id.unwrap_or(0));
    }

    let mut element = Vec::new();
    push_u16(&mut element, 0x0102);
    push_u16(&mut element, 16);
    push_u32(&mut element, 16 + 20 + 20 * attrs.len() as u32);
    push_u32(&mut element, 1); // line
    push_u32(&mut element, u32::MAX); // comment
    push_u32(&mut element, u32::MAX); // namespace
    push_u32(&mut element, manifest_index);
    push_u16(&mut element, 20); // attribute start
    push_u16(&mut element, 20); // attribute size
    push_u16(&mut element, attrs.len() as u16);
    push_u16(&mut element, 0);
    push_u16(&mut element, 0);
    push_u16(&mut element, 0);
    for (index, attr) in attrs.iter().enumerate() {
        push_u32(&mut element, u32::MAX);
        push_u32(&mut element, index as u32);
        match attr.value {
            AttrValue::Text(_) => {
                push_u32(&mut element, value_index[index]);
                push_u16(&mut element, 8);
                element.push(0);
                element.push(0x03);
                push_u32(&mut element, value_index[index]);
            }
            AttrValue::Int(value) => {
                push_u32(&mut element, u32::MAX);
                push_u16(&mut element, 8);
                element.push(0);
                element.push(0x10);
                push_u32(&mut element, value);
            }
        }
    }

    let body_len = pool_chunk.len() + map_chunk.len() + element.len();
    let mut doc = Vec::new();
    push_u16(&mut doc, 0x0003);
    push_u16(&mut doc, 8);
    push_u32(&mut doc, 8 + body_len as u32);
    doc.extend_from_slice(&pool_chunk);
    doc.extend_from_slice(&map_chunk);
    doc.extend_from_slice(&element);
    doc
}

/// A base-APK manifest with package, versionName and versionCode.
#[must_use]
pub fn base_manifest(package: &str, version_name: &str, version_code: u32) -> Vec<u8> {
    manifest(
        &[
            Attr {
                name: "versionCode",
                resource_id: Some(0x0101_021b),
                value: AttrValue::Int(version_code),
            },
            Attr {
                name: "versionName",
                resource_id: Some(0x0101_021c),
                value: AttrValue::Text(version_name),
            },
            Attr {
                name: "package",
                resource_id: None,
                value: AttrValue::Text(package),
            },
        ],
        true,
    )
}

// ------------------------------------------------------------------ config --

/// Serializes a config the way Unity lays it out (little endian).
#[must_use]
pub fn encode(settings: &PlayerSettings, layout: Layout) -> Vec<u8> {
    fn string(out: &mut Vec<u8>, value: &str) {
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i64.to_le_bytes());
    out.extend_from_slice(&[1, 0, 0, 0]);
    out.extend_from_slice(&1i32.to_le_bytes());
    out.extend_from_slice(&42i64.to_le_bytes());
    string(&mut out, &settings.name);
    for value in [
        &settings.memo,
        &settings.client_major_version,
        &settings.client_minor_version,
        &settings.client_build_version,
        &settings.snapshot,
        &settings.client_version_suffix,
        &settings.client_data_major_version,
        &settings.client_data_minor_version,
        &settings.client_data_build_version,
        &settings.client_data_revision,
        &settings.company_name,
        &settings.product_name,
        &settings.bundle_identifier,
        &settings.bundle_version,
        &settings.asset_hash,
        &settings.client_app_hash,
        &settings.ad_mob_app_id,
    ] {
        string(&mut out, value);
    }
    if layout == Layout::Android {
        out.extend_from_slice(&(settings.bundle_version_code.unwrap_or(0) as i32).to_le_bytes());
    }
    out
}

/// A plausible config named `name`.
#[must_use]
pub fn sample(name: &str, version: (&str, &str, &str), hash: &str) -> PlayerSettings {
    PlayerSettings {
        name: name.into(),
        memo: String::new(),
        client_major_version: version.0.into(),
        client_minor_version: version.1.into(),
        client_build_version: version.2.into(),
        snapshot: String::new(),
        client_version_suffix: String::new(),
        client_data_major_version: version.0.into(),
        client_data_minor_version: version.1.into(),
        client_data_build_version: "10".into(),
        client_data_revision: "0".into(),
        company_name: "Example Company".into(),
        product_name: "Example".into(),
        bundle_identifier: "com.example.game".into(),
        bundle_version: format!("{}.{}.{}", version.0, version.1, version.2),
        asset_hash: String::new(),
        client_app_hash: hash.into(),
        ad_mob_app_id: String::new(),
        bundle_version_code: name.ends_with("_android").then_some(2),
    }
}

// --------------------------------------------------------- serialized file --

/// One object of a synthetic serialized file.
pub struct Object {
    pub path_id: i64,
    pub class_id: i32,
    /// MonoBehaviours carry the hash of their script; objects sharing it share
    /// a serialized type.
    pub script_id: [u8; 16],
    pub payload: Vec<u8>,
}

/// A Unity serialized file, format 22, little endian, type trees stripped.
#[must_use]
pub fn serialized_file(unity_version: &str, objects: &[Object]) -> Vec<u8> {
    const HEADER: usize = 48;
    let mut types: Vec<(i32, [u8; 16])> = Vec::new();
    let mut type_index = Vec::new();
    for object in objects {
        let key = (object.class_id, object.script_id);
        let index = types.iter().position(|t| *t == key).unwrap_or_else(|| {
            types.push(key);
            types.len() - 1
        });
        type_index.push(index as i32);
    }

    let mut data = Vec::new();
    let mut placements = Vec::new();
    for object in objects {
        while !data.len().is_multiple_of(8) {
            data.push(0);
        }
        placements.push((data.len() as i64, object.payload.len() as u32));
        data.extend_from_slice(&object.payload);
    }

    let mut meta = Vec::new();
    meta.extend_from_slice(unity_version.as_bytes());
    meta.push(0);
    meta.extend_from_slice(&13i32.to_le_bytes()); // Android
    meta.push(0); // no type trees
    meta.extend_from_slice(&(types.len() as i32).to_le_bytes());
    for (index, (class_id, script_id)) in types.iter().enumerate() {
        meta.extend_from_slice(&class_id.to_le_bytes());
        meta.push(0);
        meta.extend_from_slice(&(index as i16).to_le_bytes());
        if *class_id == 114 {
            meta.extend_from_slice(script_id);
        }
        meta.extend_from_slice(&[0u8; 16]);
    }
    meta.extend_from_slice(&(objects.len() as i32).to_le_bytes());
    for (object, (&(start, size), &ty)) in
        objects.iter().zip(placements.iter().zip(type_index.iter()))
    {
        while !(HEADER + meta.len()).is_multiple_of(4) {
            meta.push(0);
        }
        meta.extend_from_slice(&object.path_id.to_le_bytes());
        meta.extend_from_slice(&start.to_le_bytes());
        meta.extend_from_slice(&size.to_le_bytes());
        meta.extend_from_slice(&ty.to_le_bytes());
    }
    meta.extend_from_slice(&0i32.to_le_bytes()); // script types
    meta.extend_from_slice(&0i32.to_le_bytes()); // externals
    meta.extend_from_slice(&0i32.to_le_bytes()); // reference types
    meta.push(0); // user information

    let mut data_offset = HEADER + meta.len();
    while !data_offset.is_multiple_of(16) {
        data_offset += 1;
    }
    let file_size = data_offset + data.len();
    let mut out = Vec::with_capacity(file_size);
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&22u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]); // little endian + reserved
    out.extend_from_slice(&(meta.len() as u32).to_be_bytes());
    out.extend_from_slice(&(file_size as i64).to_be_bytes());
    out.extend_from_slice(&(data_offset as i64).to_be_bytes());
    out.extend_from_slice(&0i64.to_be_bytes());
    out.extend_from_slice(&meta);
    out.resize(data_offset, 0);
    out.extend_from_slice(&data);
    out
}

// ----------------------------------------------------------------- UnityFS --

/// A UnityFS (format 8) bundle of `entries`, its data split into LZ4 blocks of
/// `block_size` bytes.
#[must_use]
pub fn unity_fs(revision: &str, entries: &[(&str, &[u8])], block_size: usize) -> Vec<u8> {
    let mut stream = Vec::new();
    let mut nodes = Vec::new();
    for (path, bytes) in entries {
        nodes.push((stream.len() as i64, bytes.len() as i64, *path));
        stream.extend_from_slice(bytes);
    }
    let mut blocks = Vec::new();
    let mut block_data = Vec::new();
    for chunk in stream.chunks(block_size.max(1)) {
        let compressed = lz4_flex::block::compress(chunk);
        blocks.push((chunk.len() as u32, compressed.len() as u32));
        block_data.extend_from_slice(&compressed);
    }

    let mut info = vec![0u8; 16];
    info.extend_from_slice(&(blocks.len() as i32).to_be_bytes());
    for (raw, packed) in &blocks {
        info.extend_from_slice(&raw.to_be_bytes());
        info.extend_from_slice(&packed.to_be_bytes());
        info.extend_from_slice(&2u16.to_be_bytes()); // LZ4
    }
    info.extend_from_slice(&(nodes.len() as i32).to_be_bytes());
    for (offset, size, path) in &nodes {
        info.extend_from_slice(&offset.to_be_bytes());
        info.extend_from_slice(&size.to_be_bytes());
        info.extend_from_slice(&4u32.to_be_bytes());
        info.extend_from_slice(path.as_bytes());
        info.push(0);
    }

    let mut header = Vec::new();
    header.extend_from_slice(b"UnityFS\0");
    header.extend_from_slice(&8u32.to_be_bytes());
    header.extend_from_slice(b"5.x.x\0");
    header.extend_from_slice(revision.as_bytes());
    header.push(0);
    let size_at = header.len();
    header.extend_from_slice(&0i64.to_be_bytes());
    header.extend_from_slice(&(info.len() as u32).to_be_bytes());
    header.extend_from_slice(&(info.len() as u32).to_be_bytes());
    header.extend_from_slice(&0x40u32.to_be_bytes()); // info follows header, uncompressed
    while !header.len().is_multiple_of(16) {
        header.push(0);
    }
    let total = header.len() + info.len() + block_data.len();
    header[size_at..size_at + 8].copy_from_slice(&(total as i64).to_be_bytes());
    let mut out = header;
    out.extend_from_slice(&info);
    out.extend_from_slice(&block_data);
    out
}

// --------------------------------------------------------------------- zip --

/// A minimal zip writer: each entry is stored or deflated.
#[must_use]
pub fn zip_archive(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data, deflate) in entries {
        let crc = crc32(data);
        let payload = if *deflate {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(data).expect("in-memory deflate");
            encoder.finish().expect("in-memory deflate")
        } else {
            data.to_vec()
        };
        let method: u16 = if *deflate { 8 } else { 0 };
        let offset = out.len() as u32;
        push_u32(&mut out, 0x0403_4b50);
        push_u16(&mut out, 20);
        push_u16(&mut out, 0);
        push_u16(&mut out, method);
        push_u32(&mut out, 0); // time/date
        push_u32(&mut out, crc);
        push_u32(&mut out, payload.len() as u32);
        push_u32(&mut out, data.len() as u32);
        push_u16(&mut out, name.len() as u16);
        push_u16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&payload);

        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, 20);
        push_u16(&mut central, 20);
        push_u16(&mut central, 0);
        push_u16(&mut central, method);
        push_u32(&mut central, 0);
        push_u32(&mut central, crc);
        push_u32(&mut central, payload.len() as u32);
        push_u32(&mut central, data.len() as u32);
        push_u16(&mut central, name.len() as u16);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    push_u32(&mut out, 0x0605_4b50);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);
    push_u16(&mut out, entries.len() as u16);
    push_u16(&mut out, entries.len() as u16);
    push_u32(&mut out, central.len() as u32);
    push_u32(&mut out, cd_offset);
    push_u16(&mut out, 0);
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

// ----------------------------------------------------------------- package --

/// What [`xapk`] put into its package.
pub struct Fixture {
    pub bytes: Vec<u8>,
    pub production: PlayerSettings,
}

/// An XAPK shaped like the Nuverse store packages: a base APK with the
/// manifest, an asset-pack APK (stored) holding `assets/bin/Data/data.unity3d`,
/// a UnityFS bundle whose `resources.assets` holds production and staging
/// configs among unrelated MonoBehaviours and other objects.
#[must_use]
pub fn xapk(package: &str, version: (&str, &str, &str), app_hash: &str) -> Fixture {
    let (bundle, production) = nuverse_bundle(package, version, app_hash);
    let base = zip_archive(&[
        (
            "AndroidManifest.xml",
            &base_manifest(package, &format!("{}.{}.0", version.0, version.1), 4242),
            true,
        ),
        ("classes.dex", b"dex\n035\0", true),
    ]);
    let pack = zip_archive(&[
        (
            "AndroidManifest.xml",
            &manifest(
                &[Attr {
                    name: "package",
                    resource_id: None,
                    value: AttrValue::Text(package),
                }],
                true,
            ),
            true,
        ),
        ("assets/bin/Data/data.unity3d", &bundle, false),
    ]);
    let bytes = zip_archive(&[
        ("manifest.json", b"{}", true),
        (&format!("{package}.apk"), &base, false),
        ("UnityDataAssetPack.apk", &pack, false),
        (
            "config.arm64_v8a.apk",
            &zip_archive(&[("lib/arm64-v8a/x.so", b"elf", true)]),
            false,
        ),
    ]);
    Fixture { bytes, production }
}

/// A single APK shaped like a QooApp base APK of a Nuverse build: the
/// manifest and `assets/bin/Data/data.unity3d` in one archive.
#[must_use]
pub fn apk(package: &str, version: (&str, &str, &str), app_hash: &str) -> Fixture {
    let (bundle, production) = nuverse_bundle(package, version, app_hash);
    let bytes = zip_archive(&[
        (
            "AndroidManifest.xml",
            &base_manifest(package, &format!("{}.{}.0", version.0, version.1), 21901),
            true,
        ),
        ("classes.dex", b"dex\n035\0", true),
        ("assets/bin/Data/data.unity3d", &bundle, false),
    ]);
    Fixture { bytes, production }
}

/// The `data.unity3d` of a Nuverse build and the production config in it.
fn nuverse_bundle(
    package: &str,
    version: (&str, &str, &str),
    app_hash: &str,
) -> (Vec<u8>, PlayerSettings) {
    let mut production = sample("production_android", version, app_hash);
    production.bundle_identifier = package.to_owned();
    let staging = sample(
        "staging1_android",
        version,
        "11111111-2222-3333-4444-555555555555",
    );
    let config_script = [7u8; 16];
    let mut objects = vec![
        Object {
            path_id: 1,
            class_id: 1, // GameObject, never a candidate
            script_id: [0; 16],
            payload: vec![0; 64],
        },
        Object {
            path_id: 2,
            class_id: 114, // an unrelated script with plausible size
            script_id: [9; 16],
            payload: (0..400u32).map(|i| (i * 31 % 251) as u8).collect(),
        },
        Object {
            path_id: 3,
            class_id: 114,
            script_id: config_script,
            payload: encode(&staging, Layout::Android),
        },
        Object {
            path_id: 4,
            class_id: 114,
            script_id: config_script,
            payload: encode(&production, Layout::Android),
        },
    ];
    // Padding objects push the configs past the first blocks, so a lazy read
    // has blocks to skip.
    for path_id in 10..40 {
        objects.insert(
            1,
            Object {
                path_id,
                class_id: 28,
                script_id: [0; 16],
                payload: (0..2048u32)
                    .map(|i| (i * path_id as u32 % 253) as u8)
                    .collect(),
            },
        );
    }
    let resources = serialized_file("2022.3.62f1", &objects);
    let globals = serialized_file("2022.3.62f1", &[]);
    let bundle = unity_fs(
        "2022.3.62f1",
        &[
            ("globalgamemanagers", &globals),
            ("resources.assets", &resources),
            ("resources.assets.resS", &[0u8; 300]),
        ],
        4096,
    );
    (bundle, production)
}
