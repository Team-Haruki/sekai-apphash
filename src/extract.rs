//! Walks an APK, XAPK or IPA (zip) and extracts the manifest and the Unity
//! player-setting configs, reading only the entries that matter.

use tracing::debug;
use unity_rs_core::source::Region;

use crate::axml::{parse_manifest, ManifestInfo};
use crate::error::Result;
use crate::unity::{self, UnityScan};
use crate::zip::{ZipArchive, ZipEntry, METHOD_STORED};

/// File names the game uses for its Unity data across regions and platforms:
/// the bundled player data of the Nuverse builds, and the hashed data files of
/// the Colorful Palette Android and iOS builds.
pub const UNITY_DATA_NAMES: [&str; 4] = [
    "data.unity3d",
    "6350e2ec327334c8a9b7f494f344a761",
    "c726e51b6fe37463685916a1687158dd",
    "resources.assets",
];

const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
/// Upper bound for inflating a compressed Unity data entry in memory.
const MAX_INFLATED_UNITY_BYTES: u64 = 512 * 1024 * 1024;
const MAX_NESTING: usize = 2;

/// Everything the package walk found.
#[derive(Debug, Clone, Default)]
pub struct PackageScan {
    /// The manifest of the base APK, when the package is an Android one.
    pub manifest: Option<ManifestInfo>,
    pub unity: UnityScan,
    /// The zip path (nested paths joined with `!/`) of the Unity file used.
    pub unity_path: Option<String>,
}

/// Scans a package held in `region`.
pub fn scan_package(region: Region) -> Result<PackageScan> {
    let archive = ZipArchive::open(region)?;
    let mut scan = PackageScan::default();
    walk(&archive, "", 0, &mut scan)?;
    if scan.unity.configs.is_empty() {
        return Err(unity::not_found("the package"));
    }
    Ok(scan)
}

/// A split APK's manifest names the package but not the version; keep looking
/// until the base APK's manifest (with `versionName`) has been read.
fn manifest_complete(scan: &PackageScan) -> bool {
    scan.manifest
        .as_ref()
        .is_some_and(|manifest| manifest.version_name.is_some())
}

fn walk(archive: &ZipArchive, prefix: &str, depth: usize, scan: &mut PackageScan) -> Result<()> {
    if !manifest_complete(scan) {
        if let Some(entry) = archive.by_name("AndroidManifest.xml") {
            match archive
                .read(entry, MAX_MANIFEST_BYTES)
                .and_then(|data| parse_manifest(&data))
            {
                Ok(manifest) => {
                    debug!(path = %format!("{prefix}AndroidManifest.xml"), package = %manifest.package, version = ?manifest.version_name, "Read manifest");
                    if scan.manifest.is_none() || manifest.version_name.is_some() {
                        scan.manifest = Some(manifest);
                    }
                }
                Err(error) => debug!(%error, "Unreadable AndroidManifest.xml"),
            }
        }
    }

    if scan.unity.configs.is_empty() {
        for entry in archive
            .entries()
            .iter()
            .filter(|entry| UNITY_DATA_NAMES.contains(&entry.base_name()))
        {
            let path = format!("{prefix}{}", entry.name);
            let region = match archive.entry_region(entry, MAX_INFLATED_UNITY_BYTES) {
                Ok(region) => region,
                Err(error) => {
                    debug!(%path, %error, "Cannot open Unity data entry");
                    continue;
                }
            };
            match unity::scan(&region) {
                Ok(found) if !found.configs.is_empty() => {
                    debug!(%path, configs = found.configs.len(), "Found player-setting configs");
                    scan.unity = found;
                    scan.unity_path = Some(path);
                    break;
                }
                Ok(_) => debug!(%path, "No configs in Unity data entry"),
                Err(error) => debug!(%path, %error, "Unity data entry not readable"),
            }
        }
    }

    if depth >= MAX_NESTING || (manifest_complete(scan) && !scan.unity.configs.is_empty()) {
        return Ok(());
    }
    let mut nested: Vec<&ZipEntry> = archive
        .entries()
        .iter()
        .filter(|entry| entry.name.to_ascii_lowercase().ends_with(".apk"))
        .collect();
    nested.sort_by_key(|entry| nested_priority(entry));
    for entry in nested {
        if manifest_complete(scan) && !scan.unity.configs.is_empty() {
            break;
        }
        let path = format!("{prefix}{}!/", entry.name);
        if entry.method != METHOD_STORED {
            debug!(%path, method = entry.method, "Skipping compressed nested APK");
            continue;
        }
        match archive.stored_region(entry).and_then(ZipArchive::open) {
            Ok(inner) => walk(&inner, &path, depth + 1, scan)?,
            Err(error) => debug!(%path, %error, "Nested APK is not a readable zip"),
        }
    }
    Ok(())
}

/// The asset pack split holds the Unity data, the base APK the manifest;
/// per-ABI/density/language config splits hold neither.
fn nested_priority(entry: &ZipEntry) -> u8 {
    let name = entry.base_name();
    if name.contains("UnityDataAssetPack") {
        0
    } else if name.starts_with("config.") {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{xapk, zip_archive};

    #[test]
    fn scans_an_xapk_with_nested_apks() {
        let fixture = xapk(
            "com.hermes.mk.asia",
            ("6", "4", "1"),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        );
        let scan = scan_package(Region::from_bytes(fixture.bytes)).unwrap();
        let manifest = scan.manifest.unwrap();
        assert_eq!(manifest.package, "com.hermes.mk.asia");
        assert_eq!(manifest.version_name.as_deref(), Some("6.4.0"));
        assert_eq!(manifest.version_code, Some(4242));
        assert_eq!(
            scan.unity_path.as_deref(),
            Some("UnityDataAssetPack.apk!/assets/bin/Data/data.unity3d")
        );
        assert_eq!(
            scan.unity.serialized_file.as_deref(),
            Some("resources.assets")
        );
        let prod = crate::unity::production(&scan.unity.configs, "android").unwrap();
        assert_eq!(prod, &fixture.production);
        assert_eq!(scan.unity.configs.len(), 2);
    }

    #[test]
    fn a_package_without_unity_data_is_an_error() {
        let apk = zip_archive(&[("classes.dex", b"dex", true)]);
        let error = scan_package(Region::from_bytes(apk)).unwrap_err();
        assert!(error.to_string().contains("PlayerSettingConfig"));
        // A compressed nested APK cannot be range-read and is skipped.
        let inner = zip_archive(&[("assets/bin/Data/data.unity3d", b"UnityFS", false)]);
        let outer = zip_archive(&[("base.apk", &inner, true)]);
        assert!(scan_package(Region::from_bytes(outer)).is_err());
    }

    #[test]
    fn prioritises_the_asset_pack_then_the_base_apk() {
        let entry = |name: &str| ZipEntry {
            name: name.into(),
            method: 0,
            flags: 0,
            crc32: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            local_header_offset: 0,
        };
        let mut entries = [
            entry("config.arm64_v8a.apk"),
            entry("com.example.apk"),
            entry("UnityDataAssetPack.apk"),
        ];
        entries.sort_by_key(nested_priority);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "UnityDataAssetPack.apk",
                "com.example.apk",
                "config.arm64_v8a.apk"
            ]
        );
    }
}
