//! The app identity reported for one region.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use tracing::warn;

use crate::axml::ManifestInfo;
use crate::error::{Error, Result};
use crate::extract::PackageScan;
use crate::player_settings::PlayerSettings;
use crate::remote::RemoteInfo;
use crate::server::ServerRegion;
use crate::unity;

/// The identity a game client of one region presents, plus where it came
/// from. `appVersion`/`appHash` are what a client sends to log in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppIdentity {
    pub region: ServerRegion,
    /// The config the identity was taken from (`production_android`).
    pub config: String,
    /// The version a client logs in with: the config's client version for
    /// JP/EN, the manifest `versionName` for TW/KR/CN (see
    /// [`login_app_version`]).
    pub app_version: String,
    /// Where `app_version` came from.
    #[serde(default)]
    pub app_version_source: AppVersionSource,
    /// `clientMajor.clientMinor.clientBuild` of the config (informational
    /// for TW/KR/CN, where it can be ahead of the version the server
    /// accepts). Empty only in state written before it existed.
    #[serde(default)]
    pub client_version: String,
    pub app_hash: String,
    /// `clientDataMajor.Minor.Build` from the same config.
    pub data_version: String,
    /// `clientMajor.clientMinor.clientDataRevision`.
    pub ab_version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub asset_hash: String,
    pub bundle_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_version_code: Option<i64>,
    pub bundle_identifier: String,
    pub product_name: String,
    pub company_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version_suffix: String,
    /// The store-facing package metadata (base APK manifest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<PackageManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unity_version: Option<String>,
    /// Where in the package the config was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unity_file: Option<String>,
    /// Every `PlayerSettingConfig` in the build (all environments).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub configs: Vec<PlayerSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceInfo>,
    pub extracted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManifest {
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_code: Option<u32>,
}

impl From<ManifestInfo> for PackageManifest {
    fn from(info: ManifestInfo) -> Self {
        Self {
            package: info.package,
            version_name: info.version_name,
            version_code: info.version_code,
        }
    }
}

/// Where an identity's `appVersion` came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppVersionSource {
    /// The config's client version (JP/EN).
    #[default]
    Config,
    /// The base APK manifest's `versionName` (TW/KR/CN).
    Manifest,
    /// TW/KR/CN without a usable manifest `versionName`: the config's client
    /// version with the patch part zeroed.
    ConfigPatchZeroed,
}

/// The package the identity was read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    /// The configured location (URL or local path).
    pub location: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Bytes actually downloaded (range requests) for this extraction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_fetched: Option<u64>,
}

impl SourceInfo {
    #[must_use]
    pub fn from_remote(info: &RemoteInfo, bytes_fetched: Option<u64>) -> Self {
        Self {
            location: info.url.clone(),
            size: info.size,
            etag: info.etag.clone(),
            last_modified: info.last_modified.clone(),
            file_name: info.file_name.clone(),
            bytes_fetched,
        }
    }
}

impl AppIdentity {
    /// Builds the identity of `region` from a package scan, checking that the
    /// manifest names one of `packages` (empty: the region's own package).
    pub fn from_scan(
        region: ServerRegion,
        platform: &str,
        scan: PackageScan,
        packages: &[String],
        source: Option<SourceInfo>,
    ) -> Result<Self> {
        if let Some(manifest) = &scan.manifest {
            let allowed = if packages.is_empty() {
                manifest.package == region.package_name()
            } else {
                packages.iter().any(|p| p == &manifest.package)
            };
            if !allowed {
                return Err(Error::Other(
                    match ServerRegion::from_package(&manifest.package) {
                        Some(found) if found != region => format!(
                            "package {} belongs to {found}, not {region}",
                            manifest.package
                        ),
                        _ => format!(
                            "package {} is not allowed for {region} (allowed: {})",
                            manifest.package,
                            if packages.is_empty() {
                                region.package_name().to_owned()
                            } else {
                                packages.join(", ")
                            }
                        ),
                    },
                ));
            }
        }
        let config = unity::production(&scan.unity.configs, platform).ok_or_else(|| {
            let names: Vec<_> = scan.unity.configs.iter().map(|c| c.name.as_str()).collect();
            Error::NotFound(format!(
                "no production_{platform} config (found: {})",
                names.join(", ")
            ))
        })?;
        // The config's bundleIdentifier is not checked: the Nuverse builds come
        // from the same Unity project and carry the JP identifier there.
        let client_version = config.app_version();
        let manifest = scan.manifest.map(PackageManifest::from);
        let (app_version, app_version_source) =
            login_app_version(region, manifest.as_ref(), &client_version);
        Ok(Self {
            region,
            config: config.name.clone(),
            app_version,
            app_version_source,
            client_version,
            app_hash: config.client_app_hash.clone(),
            data_version: config.data_version(),
            ab_version: config.ab_version(),
            asset_hash: config.asset_hash.clone(),
            bundle_version: config.bundle_version.clone(),
            bundle_version_code: config.bundle_version_code,
            bundle_identifier: config.bundle_identifier.clone(),
            product_name: config.product_name.clone(),
            company_name: config.company_name.clone(),
            version_suffix: config.client_version_suffix.clone(),
            manifest,
            unity_version: scan.unity.unity_version,
            unity_file: scan
                .unity_path
                .map(|path| match scan.unity.serialized_file {
                    Some(inner) if !inner.is_empty() => format!("{path}#{inner}"),
                    _ => path,
                }),
            configs: scan.unity.configs,
            source,
            extracted_at: Utc::now(),
        })
    }

    /// Brings an identity cached by an older version up to date: before
    /// `clientVersion` existed, `appVersion` held the config's client version
    /// for every region. Re-deriving it from the cached manifest means an
    /// unchanged package is not re-downloaded and yields the same
    /// `appVersion` a fresh extraction would.
    pub fn upgrade_cached(&mut self) {
        if !self.client_version.is_empty() {
            return;
        }
        self.client_version = std::mem::take(&mut self.app_version);
        let (app_version, source) =
            login_app_version(self.region, self.manifest.as_ref(), &self.client_version);
        self.app_version = app_version;
        self.app_version_source = source;
    }

    /// The fields that matter for change detection (not timestamps or
    /// download statistics).
    #[must_use]
    pub fn essence(&self) -> (&str, &str, &str, &str, Option<&str>) {
        (
            &self.app_version,
            &self.client_version,
            &self.app_hash,
            &self.data_version,
            self.manifest
                .as_ref()
                .and_then(|m| m.version_name.as_deref()),
        )
    }
}

/// Splits a dotted version into numeric parts; `None` when any part is not a
/// number.
#[must_use]
pub fn version_parts(version: &str) -> Option<Vec<u64>> {
    version
        .trim()
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect()
}

/// Compares two dotted numeric versions (missing parts count as zero).
#[must_use]
pub fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let (mut a, mut b) = (version_parts(a)?, version_parts(b)?);
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    Some(a.cmp(&b))
}

/// The app version a client of `region` logs in with.
///
/// JP/EN: the config's client version. TW/KR/CN: the servers accept the
/// store version, the base APK manifest's `versionName`, while the config can
/// carry a later internal build (manifest `6.4.0`, config `6.4.1`); a real
/// patch release raises the manifest version too. Without a usable manifest
/// version the config's client version with the patch zeroed is the best
/// guess, logged as a warning.
#[must_use]
pub fn login_app_version(
    region: ServerRegion,
    manifest: Option<&PackageManifest>,
    client_version: &str,
) -> (String, AppVersionSource) {
    if region.is_cp() {
        return (client_version.to_owned(), AppVersionSource::Config);
    }
    let manifest_version = manifest
        .and_then(|m| m.version_name.as_deref())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    if let Some(version) = manifest_version.filter(|v| version_parts(v).is_some()) {
        return (version.to_owned(), AppVersionSource::Manifest);
    }
    let fallback = zero_patch(client_version);
    warn!(
        %region,
        manifest_version = ?manifest_version,
        client_version,
        app_version = %fallback,
        "No usable manifest versionName; using the config version with the patch zeroed"
    );
    (fallback, AppVersionSource::ConfigPatchZeroed)
}

/// `major.minor.0` of a three-part version; anything else unchanged.
#[must_use]
pub fn zero_patch(version: &str) -> String {
    match version_parts(version).as_deref() {
        Some([major, minor, _]) => format!("{major}.{minor}.0"),
        _ => version.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::*;
    use crate::testing::sample;
    use crate::unity::UnityScan;

    fn scan(configs: Vec<PlayerSettings>, package: &str) -> PackageScan {
        scan_with(configs, package, Some("6.4.0"))
    }

    fn scan_with(
        configs: Vec<PlayerSettings>,
        package: &str,
        version_name: Option<&str>,
    ) -> PackageScan {
        PackageScan {
            manifest: Some(ManifestInfo {
                package: package.into(),
                version_name: version_name.map(Into::into),
                version_code: Some(1),
            }),
            unity: UnityScan {
                configs,
                serialized_file: Some("resources.assets".into()),
                unity_version: Some("2022.3.62f3".into()),
            },
            unity_path: Some("base.apk!/assets/bin/Data/data.unity3d".into()),
        }
    }

    #[test]
    fn picks_production_android_and_checks_the_package() {
        let mut prod = sample("production_android", ("6", "4", "1"), "prod-hash");
        prod.bundle_identifier = "com.hermes.mk.asia".into();
        let staging = sample("staging1_android", ("6", "4", "1"), "staging-hash");
        let identity = AppIdentity::from_scan(
            ServerRegion::Tw,
            "android",
            scan(vec![prod, staging], "com.hermes.mk.asia"),
            &[],
            None,
        )
        .unwrap();
        // TW logs in with the manifest version, not the config's 6.4.1.
        assert_eq!(identity.app_version, "6.4.0");
        assert_eq!(identity.app_version_source, AppVersionSource::Manifest);
        assert_eq!(identity.client_version, "6.4.1");
        assert_eq!(identity.app_hash, "prod-hash");
        assert_eq!(identity.configs.len(), 2);
        assert_eq!(
            identity.unity_file.as_deref(),
            Some("base.apk!/assets/bin/Data/data.unity3d#resources.assets")
        );
        let json = serde_json::to_value(&identity).unwrap();
        assert_eq!(json["appHash"], "prod-hash");
        assert_eq!(json["appVersion"], "6.4.0");
        assert_eq!(json["appVersionSource"], "manifest");
        assert_eq!(json["clientVersion"], "6.4.1");
        assert_eq!(json["manifest"]["versionName"], "6.4.0");

        let wrong = AppIdentity::from_scan(
            ServerRegion::Kr,
            "android",
            scan(
                vec![sample("production_android", ("6", "4", "1"), "h")],
                "com.hermes.mk.asia",
            ),
            &[],
            None,
        );
        assert!(wrong.unwrap_err().to_string().contains("belongs to tw"));

        let missing = AppIdentity::from_scan(
            ServerRegion::Tw,
            "android",
            scan(
                vec![sample("staging1_android", ("6", "4", "1"), "h")],
                "com.hermes.mk.asia",
            ),
            &[],
            None,
        );
        assert!(missing
            .unwrap_err()
            .to_string()
            .contains("staging1_android"));
    }

    #[test]
    fn store_variants_need_an_allow_list() {
        let config = || vec![sample("production_android", ("6", "4", "1"), "h")];
        let qoo = "com.hermes.mk.asia.qooapp";
        let refused =
            AppIdentity::from_scan(ServerRegion::Tw, "android", scan(config(), qoo), &[], None);
        let error = refused.unwrap_err().to_string();
        assert!(error.contains("not allowed for tw"), "{error}");

        let allowed = ["com.hermes.mk.asia".to_owned(), qoo.to_owned()];
        let identity = AppIdentity::from_scan(
            ServerRegion::Tw,
            "android",
            scan(config(), qoo),
            &allowed,
            None,
        )
        .unwrap();
        assert_eq!(identity.manifest.unwrap().package, qoo);
        // The list replaces the default: the plain package needs listing too.
        let only_qoo = [qoo.to_owned()];
        assert!(AppIdentity::from_scan(
            ServerRegion::Tw,
            "android",
            scan(config(), "com.hermes.mk.asia"),
            &only_qoo,
            None
        )
        .is_err());
        // Another region's package is still named as such.
        let error = AppIdentity::from_scan(
            ServerRegion::Tw,
            "android",
            scan(config(), "com.pjsekai.kr"),
            &allowed,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("belongs to kr"), "{error}");
    }

    #[test]
    fn compares_versions() {
        assert_eq!(compare_versions("7.0.0", "6.8.1"), Some(Ordering::Greater));
        assert_eq!(compare_versions("6.4", "6.4.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("6.4.0", "6.10.0"), Some(Ordering::Less));
        assert_eq!(compare_versions("6.4.0", "6.4.1"), Some(Ordering::Less));
        assert_eq!(compare_versions("6.x", "6.4.0"), None);
        assert_eq!(zero_patch("6.4.1"), "6.4.0");
        assert_eq!(zero_patch("6.4"), "6.4");
    }

    fn manifest(version_name: Option<&str>) -> PackageManifest {
        PackageManifest {
            package: "com.example".into(),
            version_name: version_name.map(Into::into),
            version_code: None,
        }
    }

    #[test]
    fn nuverse_regions_log_in_with_the_manifest_version() {
        for region in [ServerRegion::Tw, ServerRegion::Kr, ServerRegion::Cn] {
            // The manifest wins, a patch release included.
            assert_eq!(
                login_app_version(region, Some(&manifest(Some("6.4.0"))), "6.4.1"),
                ("6.4.0".to_owned(), AppVersionSource::Manifest)
            );
            assert_eq!(
                login_app_version(region, Some(&manifest(Some(" 5.2.2 "))), "5.2.2"),
                ("5.2.2".to_owned(), AppVersionSource::Manifest)
            );
            // No manifest, no versionName, or an unusable one: zeroed config version.
            for fallback in [None, Some(manifest(None)), Some(manifest(Some("6.4.0-qa")))] {
                assert_eq!(
                    login_app_version(region, fallback.as_ref(), "6.4.1"),
                    ("6.4.0".to_owned(), AppVersionSource::ConfigPatchZeroed)
                );
            }
        }
    }

    #[test]
    fn jp_and_en_keep_the_config_version() {
        for region in [ServerRegion::Jp, ServerRegion::En] {
            assert_eq!(
                login_app_version(region, Some(&manifest(Some("7.0.0"))), "7.0.1"),
                ("7.0.1".to_owned(), AppVersionSource::Config)
            );
            assert_eq!(
                login_app_version(region, None, "7.0.1"),
                ("7.0.1".to_owned(), AppVersionSource::Config)
            );
        }
    }

    #[test]
    fn falls_back_to_the_zeroed_config_version_without_a_manifest_version() {
        let identity = AppIdentity::from_scan(
            ServerRegion::Cn,
            "android",
            scan_with(
                vec![sample("production_android", ("6", "4", "1"), "h")],
                "com.hermes.mk.asia",
                None,
            ),
            &["com.hermes.mk.asia".to_owned()],
            None,
        )
        .unwrap();
        assert_eq!(identity.app_version, "6.4.0");
        assert_eq!(
            identity.app_version_source,
            AppVersionSource::ConfigPatchZeroed
        );
        assert_eq!(identity.client_version, "6.4.1");
    }

    #[test]
    fn identities_cached_before_client_version_are_upgraded() {
        let fresh = AppIdentity::from_scan(
            ServerRegion::Tw,
            "android",
            scan(
                vec![sample("production_android", ("6", "4", "1"), "h")],
                "com.hermes.mk.asia",
            ),
            &[],
            None,
        )
        .unwrap();
        // What 0.2.0 wrote: the config version as appVersion, no new fields.
        let mut old = serde_json::to_value(&fresh).unwrap();
        old["appVersion"] = "6.4.1".into();
        let old = old.as_object_mut().unwrap();
        old.remove("appVersionSource");
        old.remove("clientVersion");
        let mut cached: AppIdentity = serde_json::from_value(old.clone().into()).unwrap();
        assert!(cached.client_version.is_empty());
        cached.upgrade_cached();
        assert_eq!(cached, fresh);
        // Idempotent.
        cached.upgrade_cached();
        assert_eq!(cached, fresh);
    }
}
