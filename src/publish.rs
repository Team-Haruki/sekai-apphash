//! Delivers identities: Haruki-Sekai-API, JSON files, and webhooks.

use std::path::Path;
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use tracing::{info, warn};

use crate::config::{FilePublisherConfig, HarukiPublisherConfig, PublishConfig, WebhookConfig};
use crate::error::{Error, Result};
use crate::identity::{compare_versions, server_app_version, AppIdentity};
use crate::server::ServerRegion;
use crate::state::{digest, write_atomic};

pub const SIGNATURE_HEADER: &str = "X-Sekai-Apphash-Signature";
pub const EVENT_HEADER: &str = "X-Sekai-Apphash-Event";

/// What one publisher did with one identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Delivered; the target changed (or may have).
    Delivered,
    /// The target already had this identity.
    Unchanged,
    /// Not for this publisher (region filter).
    Skipped,
}

/// The configured publishers.
pub struct Publishers {
    haruki: Option<HarukiPublisher>,
    file: Option<FilePublisher>,
    webhooks: Vec<WebhookPublisher>,
}

impl Publishers {
    pub fn new(config: &PublishConfig, user_agent: &str) -> Result<Self> {
        let client = |timeout: u64| {
            reqwest::Client::builder()
                .user_agent(user_agent)
                .timeout(Duration::from_secs(timeout.max(1)))
                .build()
                .map_err(Error::from)
        };
        let haruki = match config.haruki.as_ref().filter(|c| c.enabled) {
            Some(c) => Some(HarukiPublisher {
                client: client(c.timeout_secs)?,
                config: c.clone(),
            }),
            None => None,
        };
        let file = config
            .file
            .as_ref()
            .filter(|c| c.enabled)
            .map(|c| FilePublisher { config: c.clone() });
        let webhooks = config
            .webhooks
            .iter()
            .filter(|(_, c)| c.enabled)
            .map(|(name, c)| {
                Ok(WebhookPublisher {
                    name: format!("webhook:{name}"),
                    client: client(c.timeout_secs)?,
                    config: c.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            haruki,
            file,
            webhooks,
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.haruki.is_none() && self.file.is_none() && self.webhooks.is_empty()
    }

    /// Runs every publisher for `identity`. `delivered` maps publisher names to
    /// the digest each last delivered; it is updated in place. Returns the
    /// failures (publisher name, error); one failing publisher does not stop
    /// the others.
    pub async fn publish(
        &self,
        identity: &AppIdentity,
        delivered: &mut std::collections::BTreeMap<String, String>,
        force: bool,
    ) -> Vec<(String, Error)> {
        let stable = stable_digest(identity);
        let mut failures = Vec::new();
        let mut record =
            |name: &str,
             result: Result<Outcome>,
             delivered: &mut std::collections::BTreeMap<String, String>| {
                match result {
                    Ok(Outcome::Delivered) => {
                        info!(region = %identity.region, publisher = name, app_version = %identity.app_version, "Published app identity");
                        delivered.insert(name.to_owned(), stable.clone());
                    }
                    Ok(Outcome::Unchanged) => {
                        delivered.insert(name.to_owned(), stable.clone());
                    }
                    Ok(Outcome::Skipped) => {}
                    Err(error) => {
                        warn!(region = %identity.region, publisher = name, %error, "Publishing failed");
                        failures.push((name.to_owned(), error));
                    }
                }
            };
        if let Some(haruki) = &self.haruki {
            let result = haruki.publish(identity, force).await;
            record("haruki", result, delivered);
        }
        if let Some(file) = &self.file {
            let unchanged = !force && delivered.get("file") == Some(&stable);
            let result = if unchanged {
                Ok(Outcome::Unchanged)
            } else {
                file.publish(identity).await
            };
            record("file", result, delivered);
        }
        for hook in &self.webhooks {
            let unchanged = !force && delivered.get(&hook.name) == Some(&stable);
            let result = if unchanged {
                Ok(Outcome::Unchanged)
            } else {
                hook.publish(identity).await
            };
            record(&hook.name, result, delivered);
        }
        failures
    }
}

/// Digest of the identity without volatile fields (extraction time, download
/// statistics), so re-extracting the same build is not a change.
#[must_use]
pub fn stable_digest(identity: &AppIdentity) -> String {
    let mut stable = identity.clone();
    stable.extracted_at = chrono::DateTime::<chrono::Utc>::UNIX_EPOCH;
    if let Some(source) = stable.source.as_mut() {
        source.bytes_fetched = None;
    }
    digest(&serde_json::to_vec(&stable).unwrap_or_default())
}

fn in_filter(filter: &[ServerRegion], region: ServerRegion) -> bool {
    filter.is_empty() || filter.contains(&region)
}

/// The identity fields of a node's version file. `appVersion` must be
/// present: a body without it is not a version file. An empty `appVersion`
/// means the node has no identity for the region yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeIdentity {
    pub app_version: String,
    #[serde(default)]
    pub app_hash: String,
}

impl NodeIdentity {
    /// The node reports no app version (a fresh version file).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.app_version.trim().is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

pub struct HarukiPublisher {
    client: reqwest::Client,
    config: HarukiPublisherConfig,
}

impl HarukiPublisher {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.base_url.trim_end_matches('/'))
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.config.token.expose())
    }

    /// The identity the node currently logs in with, from
    /// `GET /internal/master/{region}/version`. Only a version file is an
    /// answer (an empty `appVersion` in it means "none yet"). A transport
    /// error, a non-200 status (404: internal endpoints disabled) or an
    /// `{ok: false}` envelope (no or unreadable version file, region not
    /// configured; the node's `POST` would refuse those too) is an error.
    pub async fn current(&self, region: ServerRegion) -> Result<NodeIdentity> {
        let response = self
            .client
            .get(self.url(&format!("/internal/master/{region}/version")))
            .header(AUTHORIZATION, self.bearer())
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Publish(format!(
                "version read answered HTTP {status}"
            )));
        }
        let value: serde_json::Value = response.json().await?;
        if value.get("ok") == Some(&serde_json::Value::Bool(false)) {
            return Err(Error::Publish(format!(
                "version read failed: {}",
                value
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
            )));
        }
        Ok(serde_json::from_value(value)?)
    }

    /// Whether `expected` may be pushed over the node's `current` identity.
    /// An empty node identity passes only for a region listed in
    /// `bootstrap_regions`. Otherwise, with `refuse_downgrade`, an older or
    /// incomparable version is refused.
    fn check_current(
        &self,
        region: ServerRegion,
        current: &NodeIdentity,
        expected: &str,
    ) -> Result<()> {
        if current.is_empty() {
            if self.config.bootstrap_regions.contains(&region) {
                warn!(%region, app_version = %expected, "Node has no app identity yet; bootstrapping this region");
                return Ok(());
            }
            return Err(Error::Publish(format!(
                "not publishing: the node has no app identity for {region} yet; \
                 list it in publish.haruki.bootstrap_regions to push the first one"
            )));
        }
        if !self.config.refuse_downgrade {
            return Ok(());
        }
        match compare_versions(expected, &current.app_version) {
            Some(std::cmp::Ordering::Less) => Err(Error::Publish(format!(
                "refusing downgrade {} -> {expected}",
                current.app_version
            ))),
            Some(_) => Ok(()),
            None => Err(Error::Publish(format!(
                "not publishing: cannot compare the node's app version {:?} with {expected:?}",
                current.app_version
            ))),
        }
    }

    pub async fn publish(&self, identity: &AppIdentity, force: bool) -> Result<Outcome> {
        let region = identity.region;
        if !in_filter(&self.config.regions, region) {
            return Ok(Outcome::Skipped);
        }
        let expected = server_app_version(region, &identity.app_version);
        // Fail closed: without the node's current identity neither the
        // "already there" nor the downgrade check can run, so nothing is sent.
        // The error is logged by `Publishers::publish` and fails the run.
        let current = self.current(region).await.map_err(|error| {
            Error::Publish(format!(
                "not publishing: cannot read the node's current identity ({error})"
            ))
        })?;
        if !force && current.app_version == expected && current.app_hash == identity.app_hash {
            return Ok(Outcome::Unchanged);
        }
        self.check_current(region, &current, &expected)?;
        let response = self
            .client
            .post(self.url("/internal/app-identity"))
            .header(AUTHORIZATION, self.bearer())
            .json(&json!({
                "server": region.as_str(),
                "appVersion": identity.app_version,
                "appHash": identity.app_hash,
            }))
            .send()
            .await?;
        match response.status() {
            StatusCode::UNAUTHORIZED => {
                return Err(Error::Publish("node rejected the token (HTTP 401)".into()))
            }
            StatusCode::NOT_FOUND => return Err(Error::Publish(
                "node answered 404: internal endpoints disabled (no internal_token) or path wrong"
                    .into(),
            )),
            status if !status.is_success() => {
                return Err(Error::Publish(format!("node answered HTTP {status}")))
            }
            _ => {}
        }
        let envelope: Envelope = response.json().await?;
        if !envelope.ok {
            return Err(Error::Publish(format!(
                "node refused the identity: {} {}",
                envelope.kind.unwrap_or_default(),
                envelope.message.unwrap_or_default()
            )));
        }
        let changed = envelope
            .data
            .as_ref()
            .and_then(|d| d.get("changed"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        Ok(if changed {
            Outcome::Delivered
        } else {
            Outcome::Unchanged
        })
    }
}

pub struct FilePublisher {
    config: FilePublisherConfig,
}

impl FilePublisher {
    fn render(&self, value: &impl serde::Serialize) -> Result<Vec<u8>> {
        let mut bytes = if self.config.pretty {
            serde_json::to_vec_pretty(value)?
        } else {
            serde_json::to_vec(value)?
        };
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub async fn publish(&self, identity: &AppIdentity) -> Result<Outcome> {
        if let Some(path) = &self.config.path {
            let mut combined = read_object(path).await;
            combined.insert(identity.region.to_string(), serde_json::to_value(identity)?);
            write_atomic(path, &self.render(&combined)?).await?;
        }
        if let Some(dir) = &self.config.dir {
            let path = dir.join(format!("{}.json", identity.region));
            write_atomic(&path, &self.render(identity)?).await?;
        }
        Ok(Outcome::Delivered)
    }
}

async fn read_object(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => serde_json::Map::new(),
    }
}

pub struct WebhookPublisher {
    name: String,
    client: reqwest::Client,
    config: WebhookConfig,
}

impl WebhookPublisher {
    pub async fn publish(&self, identity: &AppIdentity) -> Result<Outcome> {
        if !in_filter(&self.config.regions, identity.region) {
            return Ok(Outcome::Skipped);
        }
        let body = serde_json::to_vec(&json!({
            "event": "app-identity",
            "region": identity.region,
            "identity": identity,
        }))?;
        let mut request = self
            .client
            .post(&self.config.url)
            .header(CONTENT_TYPE, "application/json")
            .header(EVENT_HEADER, "app-identity");
        if !self.config.bearer_token.is_empty() {
            request = request.header(
                AUTHORIZATION,
                format!("Bearer {}", self.config.bearer_token.expose()),
            );
        }
        if !self.config.hmac_secret.is_empty() {
            request = request.header(
                SIGNATURE_HEADER,
                sign(self.config.hmac_secret.expose(), &body),
            );
        }
        let status = request.body(body).send().await?.status();
        if !status.is_success() {
            return Err(Error::Publish(format!("webhook answered HTTP {status}")));
        }
        Ok(Outcome::Delivered)
    }
}

/// `sha256=<hex>` HMAC of `body`, the scheme GitHub-style receivers verify.
#[must_use]
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::sign;

    #[test]
    fn sign_matches_rfc4231() {
        // RFC 4231 test case 2 (HMAC-SHA-256).
        assert_eq!(
            sign("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
