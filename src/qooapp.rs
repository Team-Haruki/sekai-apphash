//! QooApp store API (v11): finds the current package of an app.
//!
//! `GET <check_url>` (e.g. `.../store/v11/apps/<app id>`) answers
//! `{"data": {"packageId", "apk": {"versionName", "baseApkMd5"}, "splitApks":
//! [{"url", "md5"}]}}`. When a split named like `UnityDataAssetPack` is listed
//! (the Colorful Palette builds keep their Unity data there), that split is
//! the package; otherwise it is the base APK at the source's `download_url`
//! with `{package_id}` replaced. Either way the reported MD5 identifies the
//! build, so an unchanged build costs one API request and no download.

use std::time::Duration;

use reqwest::header::HeaderMap;
use serde::Deserialize;

use crate::error::{Error, Result};

/// Placeholder in a QooApp `download_url`.
pub const PACKAGE_ID_PLACEHOLDER: &str = "{package_id}";

/// Marks the split holding the Unity data.
const ASSET_PACK_MARKER: &str = "UnityDataAssetPack";

/// Upper bound for the store API answer.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct StoreResponse {
    data: Option<StoreApp>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StoreApp {
    package_id: Option<serde_json::Value>,
    apk: StoreApk,
    split_apks: Vec<StoreSplit>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StoreApk {
    version_name: Option<String>,
    base_apk_md5: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StoreSplit {
    url: String,
    md5: Option<String>,
}

/// What the store API reported for an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The id the download URL takes (the Android package name).
    pub package_id: String,
    pub version_name: Option<String>,
    /// MD5 of the package that will be downloaded.
    pub md5: String,
    /// Where to read the package (may carry signed query parameters).
    pub download_url: String,
    /// `download_url` without query or fragment, for reports and logs.
    pub display_url: String,
    /// The package is the asset-pack split, not the base APK.
    pub asset_pack: bool,
}

impl Release {
    /// Change detection key: a different build has a different MD5.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        format!(
            "qooapp;package={};md5={};version={};asset-pack={}",
            self.package_id,
            self.md5,
            self.version_name.as_deref().unwrap_or(""),
            self.asset_pack
        )
    }
}

/// Parses a store API answer. `download_template` holds `{package_id}`.
pub fn parse(body: &[u8], download_template: &str) -> Result<Release> {
    let response: StoreResponse = serde_json::from_slice(body)
        .map_err(|e| Error::Http(format!("qooapp store answer is not the expected JSON: {e}")))?;
    let app = response
        .data
        .ok_or_else(|| Error::Http("qooapp store answer has no data".into()))?;
    let package_id = match app.package_id {
        Some(serde_json::Value::String(id)) => id,
        Some(serde_json::Value::Number(id)) => id.to_string(),
        _ => String::new(),
    };
    if package_id.is_empty()
        || !package_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(Error::Http(format!(
            "qooapp store answer has no usable packageId ({package_id:?})"
        )));
    }
    let version_name = app.apk.version_name.filter(|v| !v.trim().is_empty());
    let (download_url, md5, asset_pack) = match app
        .split_apks
        .into_iter()
        .find(|split| split.url.contains(ASSET_PACK_MARKER))
    {
        Some(split) => (split.url, split.md5.unwrap_or_default(), true),
        None => (
            download_template.replace(PACKAGE_ID_PLACEHOLDER, &package_id),
            app.apk.base_apk_md5.unwrap_or_default(),
            false,
        ),
    };
    if md5.trim().is_empty() {
        return Err(Error::Http(
            "qooapp store answer has no MD5 for the package".into(),
        ));
    }
    let display_url = download_url
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_owned();
    Ok(Release {
        package_id,
        version_name,
        md5: md5.trim().to_ascii_lowercase(),
        download_url,
        display_url,
        asset_pack,
    })
}

/// Asks the store API at `check_url` for the app's current package, giving up
/// after `timeout`.
pub async fn resolve(
    client: &reqwest::Client,
    check_url: &str,
    download_template: &str,
    headers: &HeaderMap,
    timeout: Duration,
) -> Result<Release> {
    let response = client
        .get(check_url)
        .headers(headers.clone())
        .timeout(timeout)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::Http(format!("qooapp store answered HTTP {status}")));
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
    {
        return Err(Error::Http("qooapp store answer is too large".into()));
    }
    let body = response.bytes().await?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(Error::Http("qooapp store answer is too large".into()));
    }
    parse(&body, download_template)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = "https://example.invalid/v11/apps/{package_id}/download";

    #[test]
    fn a_base_apk_comes_from_the_download_template() {
        let body = br#"{"code":0,"data":{"packageId":"com.hermes.mk.asia",
            "apk":{"versionName":"6.4.0","baseApkMd5":"ABCDEF0123"},"splitApks":[]}}"#;
        let release = parse(body, TEMPLATE).unwrap();
        assert_eq!(release.package_id, "com.hermes.mk.asia");
        assert_eq!(release.version_name.as_deref(), Some("6.4.0"));
        assert_eq!(release.md5, "abcdef0123");
        assert!(!release.asset_pack);
        assert_eq!(
            release.download_url,
            "https://example.invalid/v11/apps/com.hermes.mk.asia/download"
        );
        assert!(release.fingerprint().contains("md5=abcdef0123"));
    }

    #[test]
    fn the_asset_pack_split_wins_when_listed() {
        let body = br#"{"data":{"packageId":"com.sega.pjsekai",
            "apk":{"versionName":"7.0.0","baseApkMd5":"base"},
            "splitApks":[{"url":"https://cdn.example.invalid/config.arm64_v8a.apk","md5":"a"},
                         {"url":"https://cdn.example.invalid/UnityDataAssetPack.apk?sig=s3cret","md5":"pack"}]}}"#;
        let release = parse(body, TEMPLATE).unwrap();
        assert!(release.asset_pack);
        assert_eq!(release.md5, "pack");
        assert!(release.download_url.ends_with("?sig=s3cret"));
        assert_eq!(
            release.display_url,
            "https://cdn.example.invalid/UnityDataAssetPack.apk"
        );
    }

    #[test]
    fn unusable_answers_are_errors() {
        for body in [
            &br#"not json"#[..],
            br#"{"code":401,"message":"login"}"#,
            br#"{"data":{"apk":{"baseApkMd5":"x"}}}"#,
            br#"{"data":{"packageId":"../etc","apk":{"baseApkMd5":"x"}}}"#,
            br#"{"data":{"packageId":"com.pjsekai.kr","apk":{"versionName":"6.4.0"}}}"#,
        ] {
            assert!(
                parse(body, TEMPLATE).is_err(),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        let numeric = br#"{"data":{"packageId":12345,"apk":{"baseApkMd5":"x"}}}"#;
        assert_eq!(parse(numeric, TEMPLATE).unwrap().package_id, "12345");
    }
}
