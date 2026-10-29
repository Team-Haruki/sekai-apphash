//! Configuration: a TOML file with `SEKAI_APPHASH_*` environment overrides.
//!
//! Nested keys are separated by a double underscore in the environment, e.g.
//! `SEKAI_APPHASH_PUBLISH__HARUKI__TOKEN` sets `publish.haruki.token` and
//! `SEKAI_APPHASH_REGIONS__JP__URL` sets `regions.jp.url`. Header names of a
//! channel are matched with `_` read as `-`, so
//! `SEKAI_APPHASH_CHANNELS__QOO__HEADERS__X_USER_TOKEN` sets the `x-user-token`
//! header of the `qoo` channel.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use figment::providers::{Env, Format, Toml};
use figment::Figment;
use serde::{Deserialize, Deserializer};

use crate::error::{Error, Result};
use crate::server::ServerRegion;

pub const ENV_PREFIX: &str = "SEKAI_APPHASH_";
/// Environment variable naming the config file (not a config key).
pub const ENV_CONFIG_PATH: &str = "SEKAI_APPHASH_CONFIG";

/// A credential. Its `Debug`/`Display` never reveal the value.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.is_empty() {
            "<unset>"
        } else {
            "<redacted>"
        })
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        // Numbers and other scalars from the environment are accepted as text.
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Self(match value {
            serde_json::Value::String(s) => s,
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        }))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub log: LogConfig,
    pub http: HttpConfig,
    pub state: StateConfig,
    pub schedule: ScheduleConfig,
    pub service: ServiceConfig,
    pub regions: BTreeMap<ServerRegion, RegionConfig>,
    /// Named request header sets that sources select with `channel`.
    pub channels: BTreeMap<String, ChannelConfig>,
    pub publish: PublishConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// Level for this crate when `RUST_LOG` is unset.
    pub level: String,
    /// `text` or `json`.
    pub format: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "text".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    pub user_agent: String,
    pub connect_timeout_secs: u64,
    /// Per request, including range requests.
    pub timeout_secs: u64,
    /// Per request for the small lookups that decide what to read: the QooApp
    /// store API and the one-byte range probe (at most `timeout_secs`).
    pub lookup_timeout_secs: u64,
    /// Optional proxy for source downloads (`http://`, `socks5://`).
    pub proxy: Option<String>,
    /// Range cache page size.
    pub page_size_kib: u64,
    /// Range cache budget per package.
    pub cache_mib: u64,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            user_agent: concat!("sekai-apphash/", env!("CARGO_PKG_VERSION")).into(),
            connect_timeout_secs: 15,
            timeout_secs: 120,
            lookup_timeout_secs: 30,
            proxy: None,
            page_size_kib: 256,
            cache_mib: 128,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StateConfig {
    /// Directory for per-region state (last fingerprint, identity, deliveries).
    /// Empty disables the cache.
    pub dir: PathBuf,
}

impl Default for StateConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("state"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    pub interval_secs: u64,
    /// A random delay in `0..=jitter_secs` is added to every interval.
    pub jitter_secs: u64,
    /// Upper bound for one region's check, extraction and delivery.
    pub region_timeout_secs: u64,
    /// Waits before trying a region's sources again when every one failed
    /// with a connection, TLS or timeout error. Shared by all regions of a
    /// pass: one outage costs at most their sum per pass. Empty = no retry.
    pub retry_delays_secs: Vec<u64>,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            interval_secs: 600,
            jitter_secs: 120,
            region_timeout_secs: 900,
            retry_delays_secs: vec![30, 45, 60],
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceConfig {
    /// `host:port` for the read-only status endpoints; empty disables them.
    pub listen: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RegionConfig {
    pub enabled: bool,
    /// Shorthand for a single `direct` source: download URL of the package
    /// (APK, XAPK or IPA); must serve byte ranges. Not combined with `sources`.
    pub url: String,
    /// Extra request headers for the `url` source.
    pub headers: BTreeMap<String, String>,
    /// Which production config to report: `android` or `ios`.
    pub platform: String,
    /// Proxy for the `url` source: unset inherits `http.proxy`, `""` connects
    /// directly, anything else is a proxy URL.
    pub proxy: Option<String>,
    /// Sources tried in order until one yields the identity.
    pub sources: Vec<SourceConfig>,
}

impl Default for RegionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: String::new(),
            headers: BTreeMap::new(),
            platform: "android".into(),
            proxy: None,
            sources: Vec::new(),
        }
    }
}

impl RegionConfig {
    /// The sources in order: `sources`, or the `url` shorthand as one direct
    /// source.
    #[must_use]
    pub fn effective_sources(&self) -> Vec<SourceConfig> {
        if !self.sources.is_empty() || self.url.trim().is_empty() {
            return self.sources.clone();
        }
        vec![SourceConfig {
            download_url: self.url.clone(),
            headers: self.headers.clone(),
            proxy: self.proxy.clone(),
            ..SourceConfig::default()
        }]
    }

    /// Whether any enabled source is configured.
    #[must_use]
    pub fn has_enabled_source(&self) -> bool {
        self.effective_sources().iter().any(|s| s.enabled)
    }
}

/// How a source finds its package.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// `download_url` is the package itself (an APK mirror or the official
    /// download); changes are detected from its size, ETag and Last-Modified.
    #[default]
    Direct,
    /// A QooApp store: `check_url` is the app's v11 store API entry; the
    /// package comes from `download_url` with `{package_id}` filled in, or
    /// from the asset-pack split the API lists. Changes are detected from the
    /// MD5 the API reports.
    Qooapp,
}

impl SourceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Qooapp => "qooapp",
        }
    }
}

/// One way to obtain a region's package.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourceConfig {
    #[serde(alias = "enable")]
    pub enabled: bool,
    /// Label for logs; defaults to the channel or the kind.
    pub name: String,
    pub kind: SourceKind,
    /// Store API URL (`qooapp`).
    pub check_url: String,
    /// The package URL (`direct`), or its template with `{package_id}`
    /// (`qooapp`). Must serve byte ranges after redirects.
    #[serde(alias = "url")]
    pub download_url: String,
    /// The `channels` entry whose headers every request of this source sends.
    pub channel: String,
    /// Extra plain request headers for this source.
    pub headers: BTreeMap<String, String>,
    /// Unset inherits `http.proxy`, `""` connects directly, anything else is
    /// a proxy URL.
    pub proxy: Option<String>,
    /// Android package names this source may serve for the region; empty
    /// means only the region's own package.
    pub packages: Vec<String>,
}

impl Default for SourceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            name: String::new(),
            kind: SourceKind::Direct,
            check_url: String::new(),
            download_url: String::new(),
            channel: String::new(),
            headers: BTreeMap::new(),
            proxy: None,
            packages: Vec::new(),
        }
    }
}

impl SourceConfig {
    /// The name used in logs and errors.
    #[must_use]
    pub fn label(&self) -> &str {
        if !self.name.trim().is_empty() {
            &self.name
        } else if !self.channel.trim().is_empty() {
            &self.channel
        } else {
            self.kind.as_str()
        }
    }
}

/// A named set of request headers. Every value is treated as a credential:
/// it is never logged or printed. Put tokens in the environment, e.g.
/// `SEKAI_APPHASH_CHANNELS__QOO__HEADERS__X_USER_TOKEN`, never in the file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChannelConfig {
    /// Header name -> value; `_` in a name is read as `-`. Transport headers
    /// the client manages (`Accept-Encoding`, `Connection`, `Keep-Alive`,
    /// `Host`, `Range`, `Content-Length`) are ignored.
    pub headers: BTreeMap<String, Secret>,
    /// Headers that must be non-empty; while one is empty, the channel's
    /// sources are skipped without sending a request.
    pub required_headers: Vec<String>,
}

/// Headers a channel may not set: the HTTP client manages them.
const TRANSPORT_HEADERS: [&str; 6] = [
    "accept-encoding",
    "connection",
    "keep-alive",
    "host",
    "range",
    "content-length",
];

/// A header name as configured, normalized: lowercase, `_` read as `-`.
#[must_use]
pub fn normalize_header_name(name: &str) -> String {
    name.trim().replace('_', "-").to_ascii_lowercase()
}

impl ChannelConfig {
    /// The headers to send, normalized and without transport headers. When a
    /// name appears twice (`x-user-token` in the file, `x_user_token` from
    /// the environment), the non-empty value wins.
    pub fn resolved_headers(&self) -> Result<BTreeMap<String, Secret>> {
        let mut out: BTreeMap<String, Secret> = BTreeMap::new();
        for (name, value) in &self.headers {
            let name = normalize_header_name(name);
            if TRANSPORT_HEADERS.contains(&name.as_str()) {
                continue;
            }
            match out.get(&name) {
                Some(existing)
                    if !existing.is_empty() && !value.is_empty() && existing != value =>
                {
                    return Err(Error::Config(format!(
                        "header {name} is set twice with different values"
                    )));
                }
                Some(existing) if !existing.is_empty() => {}
                _ => {
                    out.insert(name, value.clone());
                }
            }
        }
        Ok(out)
    }

    /// Names of required headers that are empty or missing.
    #[must_use]
    pub fn missing_required(&self) -> Vec<String> {
        let headers = self.resolved_headers().unwrap_or_default();
        self.required_headers
            .iter()
            .map(|name| normalize_header_name(name))
            .filter(|name| headers.get(name).is_none_or(Secret::is_empty))
            .collect()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PublishConfig {
    pub haruki: Option<HarukiPublisherConfig>,
    pub file: Option<FilePublisherConfig>,
    pub webhooks: BTreeMap<String, WebhookConfig>,
    pub asset_updaters: BTreeMap<String, AssetUpdaterConfig>,
}

/// Haruki-Sekai-API `POST /internal/app-identity`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarukiPublisherConfig {
    pub enabled: bool,
    /// Base URL of the node, e.g. `http://127.0.0.1:9999`.
    pub base_url: String,
    /// The node's `backend.internal_token`.
    pub token: Secret,
    /// Regions to push; empty means every region that was extracted.
    pub regions: Vec<ServerRegion>,
    /// Refuse to push a version older than the node's current one. The
    /// node's identity must then be readable and comparable, or nothing is
    /// pushed.
    pub refuse_downgrade: bool,
    /// Regions the node has no app identity for yet (its version file has an
    /// empty `appVersion`): the first identity may be pushed there without
    /// the downgrade check. Remove the region once it is bootstrapped.
    pub bootstrap_regions: Vec<ServerRegion>,
    pub timeout_secs: u64,
}

impl Default for HarukiPublisherConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: String::new(),
            token: Secret::default(),
            regions: Vec::new(),
            refuse_downgrade: true,
            bootstrap_regions: Vec::new(),
            timeout_secs: 20,
        }
    }
}

/// Writes the identities as JSON files.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilePublisherConfig {
    pub enabled: bool,
    /// One JSON object keyed by region.
    pub path: Option<PathBuf>,
    /// One `<region>.json` per region.
    pub dir: Option<PathBuf>,
    pub pretty: bool,
}

impl Default for FilePublisherConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            dir: None,
            pretty: true,
        }
    }
}

/// POSTs `{"event":"app-identity","identity":{...}}` when an identity changes.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookConfig {
    pub enabled: bool,
    pub url: String,
    /// Sent as `Authorization: Bearer <token>` when set.
    pub bearer_token: Secret,
    /// When set, `X-Sekai-Apphash-Signature: sha256=<hex HMAC of the body>`.
    pub hmac_secret: Secret,
    pub regions: Vec<ServerRegion>,
    pub timeout_secs: u64,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: String::new(),
            bearer_token: Secret::default(),
            hmac_secret: Secret::default(),
            regions: Vec::new(),
            timeout_secs: 20,
        }
    }
}

/// A Haruki Sekai Asset Updater: `PUT {url}/v2/regions/{region}/app-version`
/// with `{"unity_version": ...}` when a region's Unity version changes. The
/// app version is never sent; the updater follows the master registry for it.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AssetUpdaterConfig {
    pub enabled: bool,
    /// Base URL of the updater, e.g. `http://127.0.0.1:8080`.
    pub url: String,
    /// The updater's `server.auth.bearer_token`, sent as
    /// `Authorization: Bearer <token>` when set.
    pub token: Secret,
    /// Sent as `User-Agent`; must start with the updater's
    /// `server.auth.user_agent_prefix` when it has one.
    pub user_agent: String,
    /// Regions to push; empty means every region that was checked.
    pub regions: Vec<ServerRegion>,
    pub timeout_secs: u64,
}

impl Default for AssetUpdaterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: String::new(),
            token: Secret::default(),
            user_agent: concat!("sekai-apphash/", env!("CARGO_PKG_VERSION")).to_owned(),
            regions: Vec::new(),
            timeout_secs: 20,
        }
    }
}

impl Config {
    /// Loads `path` (optional when it does not exist) and applies environment
    /// overrides.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut figment = Figment::new();
        if let Some(path) = path {
            if !path.exists() {
                return Err(Error::Config(format!("{} does not exist", path.display())));
            }
            figment = figment.merge(Toml::file(path));
        }
        figment = figment.merge(Env::prefixed(ENV_PREFIX).split("__").ignore(&["config"]));
        let config: Self = figment
            .extract()
            .map_err(|error| Error::Config(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (region, config) in &self.regions {
            if !matches!(config.platform.as_str(), "android" | "ios") {
                return Err(Error::Config(format!(
                    "regions.{region}.platform must be android or ios"
                )));
            }
            if !config.url.trim().is_empty() && !config.sources.is_empty() {
                return Err(Error::Config(format!(
                    "regions.{region}: set either url or sources, not both"
                )));
            }
            for (index, source) in config.sources.iter().enumerate() {
                self.validate_source(&format!("regions.{region}.sources[{index}]"), source)?;
            }
        }
        for (name, channel) in &self.channels {
            channel
                .resolved_headers()
                .map_err(|e| Error::Config(format!("channels.{name}: {e}")))?;
        }
        if let Some(haruki) = self.publish.haruki.as_ref().filter(|h| h.enabled) {
            if haruki.base_url.trim().is_empty() {
                return Err(Error::Config("publish.haruki.base_url is required".into()));
            }
            if haruki.token.is_empty() {
                return Err(Error::Config(
                    "publish.haruki.token is required (set SEKAI_APPHASH_PUBLISH__HARUKI__TOKEN)"
                        .into(),
                ));
            }
        }
        if let Some(file) = self.publish.file.as_ref().filter(|f| f.enabled) {
            if file.path.is_none() && file.dir.is_none() {
                return Err(Error::Config("publish.file needs path or dir".into()));
            }
        }
        for (name, hook) in &self.publish.webhooks {
            if hook.enabled && hook.url.trim().is_empty() {
                return Err(Error::Config(format!(
                    "publish.webhooks.{name}.url is required"
                )));
            }
        }
        for (name, updater) in &self.publish.asset_updaters {
            if updater.enabled && updater.url.trim().is_empty() {
                return Err(Error::Config(format!(
                    "publish.asset_updaters.{name}.url is required"
                )));
            }
        }
        if self.schedule.interval_secs == 0 {
            return Err(Error::Config(
                "schedule.interval_secs must be positive".into(),
            ));
        }
        Ok(())
    }

    fn validate_source(&self, at: &str, source: &SourceConfig) -> Result<()> {
        if source.download_url.trim().is_empty() {
            return Err(Error::Config(format!("{at}.download_url is required")));
        }
        if source.kind == SourceKind::Qooapp {
            if source.check_url.trim().is_empty() {
                return Err(Error::Config(format!(
                    "{at}.check_url is required for a qooapp source"
                )));
            }
            if !source.download_url.contains("{package_id}") {
                return Err(Error::Config(format!(
                    "{at}.download_url must contain {{package_id}}"
                )));
            }
        }
        if !source.channel.is_empty() && !self.channels.contains_key(&source.channel) {
            return Err(Error::Config(format!(
                "{at}.channel {:?} is not defined under channels",
                source.channel
            )));
        }
        Ok(())
    }

    /// Enabled regions with an enabled source, in the canonical order.
    #[must_use]
    pub fn enabled_regions(&self) -> Vec<ServerRegion> {
        ServerRegion::ALL
            .into_iter()
            .filter(|region| {
                self.regions
                    .get(region)
                    .is_some_and(|config| config.enabled && config.has_enabled_source())
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::result_large_err)] // figment::Jail closures return figment::Error
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "<redacted>");
        assert_eq!(format!("{:?}", Secret::default()), "<unset>");
        assert_eq!(secret.expose(), "hunter2");
        let config = HarukiPublisherConfig {
            token: secret,
            ..HarukiPublisherConfig::default()
        };
        assert!(!format!("{config:?}").contains("hunter2"));
    }

    // Tests that read the environment run inside figment's Jail, which holds a
    // global lock, so they never observe another test's variables.
    #[test]
    fn loads_toml_and_validates() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "c.toml",
                r#"
[regions.jp]
url = "https://example.invalid/jp.xapk"
[regions.cn]
url = "https://example.invalid/cn.apk"
enabled = false
[regions.en]
url = ""
[publish.file]
path = "out.json"
[publish.webhooks.audit]
url = "https://example.invalid/hook"
"#,
            )?;
            let config = Config::load(Some(Path::new("c.toml"))).expect("config loads");
            assert_eq!(config.enabled_regions(), vec![ServerRegion::Jp]);
            assert_eq!(config.regions[&ServerRegion::Jp].platform, "android");
            assert_eq!(config.schedule.interval_secs, 600);
            assert!(config.publish.webhooks.contains_key("audit"));
            assert!(Config::load(Some(Path::new("missing.toml"))).is_err());
            Ok(())
        });
    }

    #[test]
    fn rejects_incomplete_publishers() {
        let mut config = Config::default();
        config.publish.haruki = Some(HarukiPublisherConfig {
            base_url: "http://127.0.0.1:1".into(),
            ..HarukiPublisherConfig::default()
        });
        assert!(config.validate().unwrap_err().to_string().contains("token"));
        config.publish.haruki = None;
        config.publish.file = Some(FilePublisherConfig::default());
        assert!(config.validate().is_err());
        config.publish.file = None;
        config.regions.insert(
            ServerRegion::Kr,
            RegionConfig {
                platform: "windows".into(),
                ..RegionConfig::default()
            },
        );
        assert!(config.validate().is_err());
    }

    #[test]
    fn environment_overrides_the_file() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "c.toml",
                "[regions.kr]\nurl = \"https://example.invalid/kr\"\n[publish.haruki]\nbase_url = \"http://127.0.0.1:9\"\n",
            )?;
            jail.set_env("SEKAI_APPHASH_CONFIG", "c.toml");
            jail.set_env("SEKAI_APPHASH_PUBLISH__HARUKI__TOKEN", "from-env");
            jail.set_env("SEKAI_APPHASH_REGIONS__KR__ENABLED", "false");
            jail.set_env("SEKAI_APPHASH_SCHEDULE__JITTER_SECS", "5");
            let config = Config::load(Some(Path::new("c.toml"))).expect("config loads");
            let haruki = config.publish.haruki.as_ref().expect("haruki section");
            assert_eq!(haruki.token.expose(), "from-env");
            assert!(config.enabled_regions().is_empty());
            assert_eq!(config.schedule.jitter_secs, 5);
            Ok(())
        });
    }

    #[test]
    fn the_example_config_is_valid() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("example.toml", include_str!("../config.example.toml"))?;
            jail.set_env(
                "SEKAI_APPHASH_PUBLISH__ASSET_UPDATERS__EXAMPLE__TOKEN",
                "updater-token",
            );
            let config = Config::load(Some(Path::new("example.toml"))).expect("example loads");
            assert_eq!(config.enabled_regions(), ServerRegion::ALL.to_vec());
            assert!(config.publish.haruki.is_some_and(|h| !h.enabled));
            assert!(config.publish.webhooks.contains_key("example"));
            let updater = &config.publish.asset_updaters["example"];
            assert!(!updater.enabled);
            assert_eq!(updater.token.expose(), "updater-token");
            Ok(())
        });
    }

    #[test]
    fn sources_and_channels_load_with_the_token_from_the_environment() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "c.toml",
                r#"
[regions.tw]
[[regions.tw.sources]]
name = "apkpure"
url = "https://example.invalid/tw.xapk"
[[regions.tw.sources]]
enable = false
kind = "qooapp"
check_url = "https://store.example.invalid/store/v11/apps/1"
download_url = "https://dl.example.invalid/v11/apps/{package_id}/download"
channel = "qoo"
proxy = ""
packages = ["com.hermes.mk.asia", "com.hermes.mk.asia.qooapp"]

[regions.kr]
url = "https://example.invalid/kr.xapk"
proxy = ""

[channels.qoo]
required_headers = ["x-user-token"]
[channels.qoo.headers]
Accept = "*/*"
Accept-Encoding = "gzip, deflate, br"
x-device-id = ""
x-user-token = ""
"#,
            )?;
            jail.set_env(
                "SEKAI_APPHASH_CHANNELS__QOO__HEADERS__X_USER_TOKEN",
                "token-from-env",
            );
            let config = Config::load(Some(Path::new("c.toml"))).expect("config loads");
            let tw = &config.regions[&ServerRegion::Tw];
            assert_eq!(tw.sources.len(), 2);
            assert_eq!(tw.sources[0].kind, SourceKind::Direct);
            assert_eq!(tw.sources[0].label(), "apkpure");
            assert!(!tw.sources[1].enabled);
            assert_eq!(tw.sources[1].label(), "qoo");
            assert_eq!(tw.sources[1].proxy.as_deref(), Some(""));
            // The url shorthand is one direct source.
            let kr = config.regions[&ServerRegion::Kr].effective_sources();
            assert_eq!(kr.len(), 1);
            assert_eq!(kr[0].download_url, "https://example.invalid/kr.xapk");
            assert_eq!(kr[0].proxy.as_deref(), Some(""));

            let qoo = &config.channels["qoo"];
            let headers = qoo.resolved_headers().unwrap();
            assert_eq!(headers["x-user-token"].expose(), "token-from-env");
            assert_eq!(headers["accept"].expose(), "*/*");
            assert!(!headers.contains_key("accept-encoding"));
            assert!(qoo.missing_required().is_empty());
            assert!(!format!("{config:?}").contains("token-from-env"));

            jail.set_env("SEKAI_APPHASH_CHANNELS__QOO__HEADERS__X_USER_TOKEN", "");
            let config = Config::load(Some(Path::new("c.toml"))).expect("config loads");
            assert_eq!(config.channels["qoo"].missing_required(), ["x-user-token"]);
            Ok(())
        });
    }

    #[test]
    fn rejects_inconsistent_sources() {
        let source = |kind, check: &str, download: &str, channel: &str| SourceConfig {
            kind,
            check_url: check.into(),
            download_url: download.into(),
            channel: channel.into(),
            ..SourceConfig::default()
        };
        let template = "https://example.invalid/{package_id}";
        let cases = [
            (
                source(SourceKind::Direct, "", "", ""),
                "download_url is required",
            ),
            (
                source(SourceKind::Qooapp, "", template, ""),
                "check_url is required",
            ),
            (
                source(
                    SourceKind::Qooapp,
                    "https://x.invalid",
                    "https://x.invalid/a",
                    "",
                ),
                "{package_id}",
            ),
            (
                source(SourceKind::Direct, "", "https://x.invalid", "nope"),
                "not defined under channels",
            ),
        ];
        for (source, expected) in cases {
            let mut config = Config::default();
            config.regions.insert(
                ServerRegion::Jp,
                RegionConfig {
                    sources: vec![source],
                    ..RegionConfig::default()
                },
            );
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(expected), "{error}");
        }

        let mut config = Config::default();
        config.regions.insert(
            ServerRegion::Jp,
            RegionConfig {
                url: "https://x.invalid".into(),
                sources: vec![source(SourceKind::Direct, "", "https://y.invalid", "")],
                ..RegionConfig::default()
            },
        );
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("not both"));

        let mut config = Config::default();
        config.channels.insert(
            "qoo".into(),
            ChannelConfig {
                headers: [
                    ("x-user-token".to_owned(), Secret::new("a")),
                    ("x_user_token".to_owned(), Secret::new("b")),
                ]
                .into(),
                required_headers: Vec::new(),
            },
        );
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("set twice"), "{error}");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("c.toml", "[regions.jp]\nurl = \"x\"\nmirror = \"y\"\n")?;
            assert!(Config::load(Some(Path::new("c.toml"))).is_err());
            jail.create_file("d.toml", "")?;
            jail.set_env("SEKAI_APPHASH_BOGUS", "1");
            assert!(Config::load(Some(Path::new("d.toml"))).is_err());
            Ok(())
        });
    }
}
