//! The `Sekai.PlayerSettingConfig` MonoBehaviours the game client ships.
//!
//! Each build carries one config object per environment and platform
//! (`production_android`, `production_ios`, `staging1_android`, ...). Player
//! builds strip type trees, so the layout is supplied here as an external
//! `TypeTree` and the object is read with `unity-rs`'s type-tree reader, which
//! also verifies that the tree consumes the object exactly.

use serde::{Deserialize, Serialize};
use unity_rs_core::serialized::{TypeTree, TypeTreeNode};
use unity_rs_core::type_tree::{TypeField, TypeValue};

/// Script-defined string fields, in serialization order.
pub const STRING_FIELDS: [&str; 17] = [
    "memo",
    "clientMajorVersion",
    "clientMinorVersion",
    "clientBuildVersion",
    "snapshot",
    "clientVersionSuffix",
    "clientDataMajorVersion",
    "clientDataMinorVersion",
    "clientDataBuildVersion",
    "clientDataRevision",
    "companyName",
    "productName",
    "bundleIdentifier",
    "bundleVersion",
    "assetHash",
    "clientAppHash",
    "adMobAppId",
];

const ALIGN: i32 = 0x4000;

/// The two script layouts: the Android subclass appends `bundleVersionCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    Android,
    Base,
}

fn node(type_name: &str, field_name: &str, level: u32, byte_size: i32, meta: i32) -> TypeTreeNode {
    TypeTreeNode {
        type_name: type_name.to_owned(),
        field_name: field_name.to_owned(),
        byte_size,
        index: 0,
        type_flags: i32::from(type_name == "Array"),
        version: 1,
        meta_flags: meta,
        level,
        type_string_offset: None,
        name_string_offset: None,
        reference_type_hash: 0,
    }
}

fn push_string(nodes: &mut Vec<TypeTreeNode>, name: &str) {
    nodes.push(node("string", name, 1, -1, 0));
    nodes.push(node("Array", "Array", 2, -1, ALIGN));
    nodes.push(node("int", "size", 3, 4, 0));
    nodes.push(node("char", "data", 3, 1, 0));
}

/// The complete object `TypeTree` (MonoBehaviour header + script fields).
#[must_use]
pub fn type_tree(layout: Layout) -> TypeTree {
    let mut nodes = vec![
        node("MonoBehaviour", "Base", 0, -1, 0),
        node("PPtr<GameObject>", "m_GameObject", 1, 12, 0),
        node("int", "m_FileID", 2, 4, 0),
        node("SInt64", "m_PathID", 2, 8, 0),
        node("UInt8", "m_Enabled", 1, 1, ALIGN),
        node("PPtr<MonoScript>", "m_Script", 1, 12, 0),
        node("int", "m_FileID", 2, 4, 0),
        node("SInt64", "m_PathID", 2, 8, 0),
    ];
    push_string(&mut nodes, "m_Name");
    for field in STRING_FIELDS {
        push_string(&mut nodes, field);
    }
    if layout == Layout::Android {
        nodes.push(node("int", "bundleVersionCode", 1, 4, 0));
    }
    TypeTree {
        nodes,
        string_buffer: Vec::new(),
    }
}

/// One decoded `PlayerSettingConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerSettings {
    /// The object name, e.g. `production_android`.
    pub name: String,
    pub memo: String,
    pub client_major_version: String,
    pub client_minor_version: String,
    pub client_build_version: String,
    pub snapshot: String,
    pub client_version_suffix: String,
    pub client_data_major_version: String,
    pub client_data_minor_version: String,
    pub client_data_build_version: String,
    pub client_data_revision: String,
    pub company_name: String,
    pub product_name: String,
    pub bundle_identifier: String,
    pub bundle_version: String,
    pub asset_hash: String,
    pub client_app_hash: String,
    pub ad_mob_app_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_version_code: Option<i64>,
}

impl PlayerSettings {
    /// `clientMajor.clientMinor.clientBuild`.
    #[must_use]
    pub fn app_version(&self) -> String {
        format!(
            "{}.{}.{}",
            self.client_major_version, self.client_minor_version, self.client_build_version
        )
    }

    /// `clientDataMajor.clientDataMinor.clientDataBuild`.
    #[must_use]
    pub fn data_version(&self) -> String {
        format!(
            "{}.{}.{}",
            self.client_data_major_version,
            self.client_data_minor_version,
            self.client_data_build_version
        )
    }

    /// The asset-bundle version: `clientMajor.clientMinor.clientDataRevision`.
    #[must_use]
    pub fn ab_version(&self) -> String {
        format!(
            "{}.{}.{}",
            self.client_major_version, self.client_minor_version, self.client_data_revision
        )
    }

    /// The platform suffix of the object name (`android`, `ios`).
    #[must_use]
    pub fn platform(&self) -> Option<&str> {
        self.name.rsplit_once('_').map(|(_, platform)| platform)
    }

    /// The environment prefix of the object name (`production`, `staging1`).
    #[must_use]
    pub fn environment(&self) -> &str {
        self.name
            .rsplit_once('_')
            .map_or(self.name.as_str(), |(env, _)| env)
    }

    /// Accepts only objects that look like a real config: numeric version
    /// parts and a non-empty app hash.
    #[must_use]
    pub fn is_plausible(&self) -> bool {
        let numeric =
            |s: &str| !s.is_empty() && s.len() <= 6 && s.bytes().all(|b| b.is_ascii_digit());
        numeric(&self.client_major_version)
            && numeric(&self.client_minor_version)
            && numeric(&self.client_build_version)
            && !self.client_app_hash.trim().is_empty()
            && self.platform().is_some()
    }

    /// Builds the struct from a value read through [`type_tree`].
    #[must_use]
    pub fn from_value(value: &TypeValue) -> Option<Self> {
        let TypeValue::Object(fields) = value else {
            return None;
        };
        let text = |name: &str| -> Option<String> {
            match field(fields, name)? {
                TypeValue::String(s) => Some(s.clone()),
                _ => None,
            }
        };
        let bundle_version_code = match field(fields, "bundleVersionCode") {
            Some(TypeValue::Signed(v)) => Some(*v),
            Some(TypeValue::Unsigned(v)) => i64::try_from(*v).ok(),
            _ => None,
        };
        Some(Self {
            name: text("m_Name")?,
            memo: text("memo")?,
            client_major_version: text("clientMajorVersion")?,
            client_minor_version: text("clientMinorVersion")?,
            client_build_version: text("clientBuildVersion")?,
            snapshot: text("snapshot")?,
            client_version_suffix: text("clientVersionSuffix")?,
            client_data_major_version: text("clientDataMajorVersion")?,
            client_data_minor_version: text("clientDataMinorVersion")?,
            client_data_build_version: text("clientDataBuildVersion")?,
            client_data_revision: text("clientDataRevision")?,
            company_name: text("companyName")?,
            product_name: text("productName")?,
            bundle_identifier: text("bundleIdentifier")?,
            bundle_version: text("bundleVersion")?,
            asset_hash: text("assetHash")?,
            client_app_hash: text("clientAppHash")?,
            ad_mob_app_id: text("adMobAppId")?,
            bundle_version_code,
        })
    }
}

fn field<'a>(fields: &'a [TypeField], name: &str) -> Option<&'a TypeValue> {
    fields.iter().find(|f| f.name == name).map(|f| &f.value)
}

#[cfg(test)]
mod tests {
    use unity_rs_core::endian::{Endian, EndianReader};
    use unity_rs_core::source::Region;
    use unity_rs_core::type_tree::{
        read_type_tree_from_reader_with_reference_types, TypeTreeReadLimits,
    };

    use super::*;
    use crate::testing::{encode, sample};

    fn read(bytes: Vec<u8>, layout: Layout) -> unity_rs_core::Result<TypeValue> {
        let region = Region::from_bytes(bytes);
        read_type_tree_from_reader_with_reference_types(
            &type_tree(layout),
            EndianReader::new(region.cursor(), Endian::Little),
            0,
            TypeTreeReadLimits::default(),
            &[],
        )
    }

    #[test]
    fn round_trips_both_layouts() {
        let android = sample("production_android", ("7", "0", "0"), "hash-a");
        let value = read(encode(&android, Layout::Android), Layout::Android).unwrap();
        assert_eq!(PlayerSettings::from_value(&value).unwrap(), android);

        let mut ios = sample("production_ios", ("7", "0", "0"), "hash-i");
        ios.bundle_version_code = None;
        let value = read(encode(&ios, Layout::Base), Layout::Base).unwrap();
        assert_eq!(PlayerSettings::from_value(&value).unwrap(), ios);
    }

    #[test]
    fn a_mismatched_layout_is_refused_not_misread() {
        let android = sample("production_android", ("7", "0", "0"), "hash-a");
        assert!(read(encode(&android, Layout::Android), Layout::Base).is_err());
        let ios = sample("production_ios", ("7", "0", "0"), "hash-i");
        assert!(read(encode(&ios, Layout::Base), Layout::Android).is_err());
    }

    #[test]
    fn derived_versions_and_names() {
        let mut s = sample("staging1_android", ("6", "4", "1"), "h");
        s.client_data_revision = "0".into();
        assert_eq!(s.app_version(), "6.4.1");
        assert_eq!(s.data_version(), "6.4.10");
        assert_eq!(s.ab_version(), "6.4.0");
        assert_eq!(s.platform(), Some("android"));
        assert_eq!(s.environment(), "staging1");
        assert!(s.is_plausible());
        s.client_major_version = "x".into();
        assert!(!s.is_plausible());
        assert_eq!(PlayerSettings::from_value(&TypeValue::Signed(1)), None);
    }
}
