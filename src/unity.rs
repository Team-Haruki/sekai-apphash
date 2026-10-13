//! Locates and decodes the `PlayerSettingConfig` objects in Unity data.

use std::collections::BTreeMap;
use std::sync::Arc;

use tracing::debug;
use unity_rs_core::bundle::{StorageBlock, UnityFsBundle};
use unity_rs_core::endian::{Endian, EndianReader};
use unity_rs_core::monobehaviour::MONO_BEHAVIOUR_CLASS_ID;
use unity_rs_core::serialized::{SerializedFile, SerializedOpenOptions};
use unity_rs_core::source::Region;
use unity_rs_core::type_tree::{
    read_type_tree_from_reader_with_reference_types, TypeTreeReadLimits,
};
use unity_rs_core::unity_version::UnityVersion;

use crate::bundle_source::{supports_lazy_entries, BundleEntrySource};
use crate::error::{Error, Result};
use crate::player_settings::{type_tree, Layout, PlayerSettings};

/// Config objects are a few hundred bytes; anything far outside is skipped
/// without being read.
const MIN_CONFIG_BYTES: u64 = 96;
const MAX_CONFIG_BYTES: u64 = 16 * 1024;
/// Upper bound for materializing a bundle entry that cannot be read lazily.
const MAX_MATERIALIZED_ENTRY: u64 = 512 * 1024 * 1024;

/// What one Unity file yielded.
#[derive(Debug, Clone, Default)]
pub struct UnityScan {
    pub configs: Vec<PlayerSettings>,
    /// The serialized file the configs came from (`resources.assets`, ...).
    pub serialized_file: Option<String>,
    pub unity_version: Option<String>,
}

/// Scans a UnityFS bundle or a bare serialized file for config objects.
pub fn scan(region: &Region) -> Result<UnityScan> {
    let mut magic = [0u8; 8];
    if region.len() >= 8 {
        region.read_exact_at(0, &mut magic)?;
    }
    if magic.starts_with(b"UnityFS\0") {
        return scan_bundle(region);
    }
    let (configs, unity_version) = scan_serialized(region.clone(), None)?;
    Ok(UnityScan {
        serialized_file: (!configs.is_empty()).then(String::new),
        configs,
        unity_version: Some(unity_version),
    })
}

fn is_serialized_entry(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    !(lower.ends_with(".ress")
        || lower.ends_with(".resource")
        || lower.ends_with(".bytes")
        || lower.contains(".ress"))
}

fn entry_priority(path: &str) -> u8 {
    // Configs live under Resources/, i.e. in resources.assets; try it first.
    match path {
        "resources.assets" => 0,
        p if p.starts_with("sharedassets") => 1,
        "globalgamemanagers.assets" => 2,
        _ => 3,
    }
}

fn scan_bundle(region: &Region) -> Result<UnityScan> {
    let bundle = UnityFsBundle::open(region)?;
    let revision = bundle.header.common.unity_revision.clone();
    let hint = (!revision.is_stripped()).then(|| revision.clone());
    let blocks: Arc<[StorageBlock]> = Arc::from(bundle.blocks.clone());
    let lazy = supports_lazy_entries(&bundle);
    debug!(
        entries = bundle.entries.len(),
        blocks = bundle.blocks.len(),
        lazy,
        revision = %revision,
        "Opened UnityFS bundle"
    );
    let mut order: Vec<usize> = (0..bundle.entries.len())
        .filter(|&i| is_serialized_entry(&bundle.entries[i].path))
        .collect();
    order.sort_by_key(|&i| (entry_priority(&bundle.entries[i].path), i));
    for index in order {
        let entry = &bundle.entries[index];
        let entry_region = if lazy {
            BundleEntrySource::entry_region(region, &bundle, &blocks, index)?
        } else if entry.size <= MAX_MATERIALIZED_ENTRY {
            Region::from_bytes(bundle.read_entry(index)?)
        } else {
            debug!(path = %entry.path, size = entry.size, "Skipping oversized bundle entry");
            continue;
        };
        match scan_serialized(entry_region, hint.clone()) {
            Ok((configs, unity_version)) if !configs.is_empty() => {
                return Ok(UnityScan {
                    configs,
                    serialized_file: Some(entry.path.clone()),
                    unity_version: Some(unity_version),
                });
            }
            Ok(_) => debug!(path = %entry.path, "No config objects in bundle entry"),
            Err(error) => {
                debug!(path = %entry.path, %error, "Bundle entry is not a readable serialized file")
            }
        }
    }
    Ok(UnityScan {
        unity_version: Some(revision.to_string()),
        ..UnityScan::default()
    })
}

/// Reads every `PlayerSettingConfig` in one serialized file.
///
/// The script's type is not known by name (player builds strip MonoScript
/// type trees, and resolving `m_Script` would need other files), so
/// MonoBehaviours of plausible size are grouped by serialized type, one object
/// per type is tried against the config layouts, and every object of a type
/// that matches is decoded.
pub fn scan_serialized(
    region: Region,
    bundle_version_hint: Option<UnityVersion>,
) -> Result<(Vec<PlayerSettings>, String)> {
    let file = SerializedFile::open_with_options(
        region,
        SerializedOpenOptions {
            bundle_version_hint,
            ..SerializedOpenOptions::default()
        },
    )?;
    let mut groups: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
    for (index, object) in file.objects.iter().enumerate() {
        if object.class_id == MONO_BEHAVIOUR_CLASS_ID
            && (MIN_CONFIG_BYTES..=MAX_CONFIG_BYTES).contains(&object.byte_size)
        {
            groups.entry(object.type_id).or_default().push(index);
        }
    }
    // Visit types in file order so neighbouring probes share decoded blocks.
    let mut ordered: Vec<Vec<usize>> = groups.into_values().collect();
    ordered.sort_by_key(|group| file.objects[group[0]].byte_start);
    debug!(
        objects = file.objects.len(),
        candidate_types = ordered.len(),
        unity = %file.unity_version_string,
        "Scanning serialized file"
    );

    let mut configs = Vec::new();
    for group in ordered {
        let Some((first, layout)) = try_decode_any(&file, group[0]) else {
            continue;
        };
        configs.push(first);
        for &index in &group[1..] {
            if let Some(settings) = decode(&file, index, layout) {
                configs.push(settings);
            }
        }
    }
    configs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((configs, file.unity_version_string.clone()))
}

fn try_decode_any(file: &SerializedFile, index: usize) -> Option<(PlayerSettings, Layout)> {
    [Layout::Android, Layout::Base]
        .into_iter()
        .find_map(|layout| decode(file, index, layout).map(|settings| (settings, layout)))
}

fn decode(file: &SerializedFile, index: usize, layout: Layout) -> Option<PlayerSettings> {
    let object = &file.objects[index];
    let payload = file.object_region(index).ok()?;
    let endian = if file.header.endianness == 0 {
        Endian::Little
    } else {
        Endian::Big
    };
    let limits = TypeTreeReadLimits {
        maximum_string_bytes: 4096,
        maximum_materialized_bytes: 1024 * 1024,
        ..TypeTreeReadLimits::default()
    };
    let value = read_type_tree_from_reader_with_reference_types(
        &type_tree(layout),
        EndianReader::new(payload.cursor(), endian),
        object.byte_start,
        limits,
        &file.reference_types,
    )
    .ok()?;
    PlayerSettings::from_value(&value).filter(PlayerSettings::is_plausible)
}

/// Picks the config a client of `platform` in the production environment uses.
#[must_use]
pub fn production<'a>(configs: &'a [PlayerSettings], platform: &str) -> Option<&'a PlayerSettings> {
    configs
        .iter()
        .find(|c| c.environment() == "production" && c.platform() == Some(platform))
}

/// Shorthand for "no config was found in this data".
pub(crate) fn not_found(what: &str) -> Error {
    Error::NotFound(format!("no PlayerSettingConfig objects in {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{encode, sample, serialized_file, unity_fs, Object};

    fn objects() -> Vec<Object> {
        let prod = sample("production_android", ("7", "0", "0"), "prod");
        let mut ios = sample("production_ios", ("7", "0", "0"), "prod-ios");
        ios.bundle_version_code = None;
        vec![
            Object {
                path_id: 1,
                class_id: 114,
                script_id: [1; 16],
                payload: vec![0xAB; 300],
            },
            Object {
                path_id: 2,
                class_id: 114,
                script_id: [2; 16],
                payload: encode(&prod, Layout::Android),
            },
            Object {
                path_id: 3,
                class_id: 114,
                script_id: [3; 16],
                payload: encode(&ios, Layout::Base),
            },
            Object {
                path_id: 4,
                class_id: 114,
                script_id: [4; 16],
                payload: vec![0; 20],
            },
            Object {
                path_id: 5,
                class_id: 49,
                script_id: [0; 16],
                payload: encode(&prod, Layout::Android),
            },
        ]
    }

    #[test]
    fn finds_both_layouts_in_a_bare_serialized_file() {
        let file = serialized_file("2022.3.62f1", &objects());
        let scan = scan(&Region::from_bytes(file)).unwrap();
        let names: Vec<_> = scan.configs.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["production_android", "production_ios"]);
        assert_eq!(scan.unity_version.as_deref(), Some("2022.3.62f1"));
        assert_eq!(
            production(&scan.configs, "ios").unwrap().client_app_hash,
            "prod-ios"
        );
        assert!(production(&scan.configs, "android")
            .unwrap()
            .bundle_version_code
            .is_some());
    }

    #[test]
    fn reads_lazily_through_lz4_bundle_blocks() {
        let resources = serialized_file("2022.3.62f1", &objects());
        let empty = serialized_file("2022.3.62f1", &[]);
        let bundle = unity_fs(
            "2022.3.62f1",
            &[
                ("globalgamemanagers", &empty),
                ("resources.assets", &resources),
                ("x.resS", b"raw"),
            ],
            512,
        );
        let scan = scan(&Region::from_bytes(bundle)).unwrap();
        assert_eq!(scan.serialized_file.as_deref(), Some("resources.assets"));
        assert_eq!(scan.configs.len(), 2);
    }

    #[test]
    fn data_without_configs_yields_none() {
        let file = serialized_file("2022.3.62f1", &objects()[..1]);
        assert!(scan(&Region::from_bytes(file)).unwrap().configs.is_empty());
        let bundle = unity_fs(
            "2022.3.62f1",
            &[("resources.assets", b"not a serialized file")],
            64,
        );
        let found = scan(&Region::from_bytes(bundle)).unwrap();
        assert!(found.configs.is_empty());
        assert_eq!(found.unity_version.as_deref(), Some("2022.3.62f1"));
        assert!(scan(&Region::from_bytes(vec![1, 2, 3])).is_err());
        assert!(not_found("x").to_string().contains("x"));
    }
}
