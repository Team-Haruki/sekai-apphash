//! Configuration: a TOML file with `SEKAI_APPHASH_*` environment overrides.
//!
//! Nested keys are separated by a double underscore in the environment, e.g.
//! `SEKAI_APPHASH_PUBLISH__HARUKI__TOKEN` sets `publish.haruki.token` and
//! `SEKAI_APPHASH_REGIONS__JP__URL` sets `regions.jp.url`.

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
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            interval_secs: 600,
            jitter_secs: 120,
            region_timeout_secs: 900,
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
    /// Download URL of the package (APK, XAPK or IPA); must serve byte ranges.
    pub url: String,
    /// Extra request headers for this source.
    pub headers: BTreeMap<String, String>,
    /// Which production config to report: `android` or `ios`.
    pub platform: String,
}

impl Default for RegionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            url: String::new(),
            headers: BTreeMap::new(),
            platform: "android".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PublishConfig {
    pub haruki: Option<HarukiPublisherConfig>,
    pub file: Option<FilePublisherConfig>,
    pub webhooks: BTreeMap<String, WebhookConfig>,
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
    /// Refuse to push a version older than the node's current one.
    pub refuse_downgrade: bool,
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
        for (region, source) in &self.regions {
            if !matches!(source.platform.as_str(), "android" | "ios") {
                return Err(Error::Config(format!(
                    "regions.{region}.platform must be android or ios"
                )));
            }
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
        if self.schedule.interval_secs == 0 {
            return Err(Error::Config(
                "schedule.interval_secs must be positive".into(),
            ));
        }
        Ok(())
    }

    /// Enabled regions with a source, in the canonical order.
    #[must_use]
    pub fn enabled_regions(&self) -> Vec<ServerRegion> {
        ServerRegion::ALL
            .into_iter()
            .filter(|region| {
                self.regions
                    .get(region)
                    .is_some_and(|source| source.enabled && !source.url.trim().is_empty())
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
            let config = Config::load(Some(Path::new("example.toml"))).expect("example loads");
            assert_eq!(config.enabled_regions(), ServerRegion::ALL.to_vec());
            assert!(config.publish.haruki.is_some_and(|h| !h.enabled));
            assert!(config.publish.webhooks.contains_key("example"));
            Ok(())
        });
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
