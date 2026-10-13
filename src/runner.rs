//! One pass over the configured regions: probe, extract when the package
//! changed, publish.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use tokio::runtime::Handle;
use tracing::{info, warn};
use unity_rs_core::source::Region;

use crate::config::{Config, RegionConfig};
use crate::error::{Error, Result};
use crate::extract::scan_package;
use crate::identity::{AppIdentity, SourceInfo};
use crate::publish::Publishers;
use crate::remote::{self, RemoteInfo};
use crate::server::ServerRegion;
use crate::state::StateStore;

/// The result of one region in one pass.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionReport {
    pub region: ServerRegion,
    /// True when the package was downloaded and parsed in this pass (false
    /// when the cached identity of an unchanged package was reused).
    pub extracted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<AppIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Publisher failures (name: message).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub publish_errors: Vec<String>,
    pub checked_at: DateTime<Utc>,
}

impl RegionReport {
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.error.is_none() && self.publish_errors.is_empty()
    }
}

/// The JSON document the CLI prints.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    pub generated_at: DateTime<Utc>,
    pub identities: BTreeMap<ServerRegion, AppIdentity>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub errors: BTreeMap<ServerRegion, String>,
}

impl Output {
    #[must_use]
    pub fn from_reports(reports: &[RegionReport]) -> Self {
        let mut identities = BTreeMap::new();
        let mut errors = BTreeMap::new();
        for report in reports {
            if let Some(identity) = &report.identity {
                identities.insert(report.region, identity.clone());
            }
            let mut messages: Vec<String> = report.error.iter().cloned().collect();
            messages.extend(report.publish_errors.iter().cloned());
            if !messages.is_empty() {
                errors.insert(report.region, messages.join("; "));
            }
        }
        Self {
            generated_at: Utc::now(),
            identities,
            errors,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// Re-extract and re-deliver even when nothing changed.
    pub force: bool,
    /// Run the publishers.
    pub publish: bool,
}

pub struct Runner {
    config: Config,
    client: reqwest::Client,
    /// Clients of regions whose `proxy` differs from `http.proxy`.
    region_clients: BTreeMap<ServerRegion, reqwest::Client>,
    publishers: Publishers,
    state: StateStore,
}

/// Builds a source client. `proxy`: `None` = no explicit proxy, `Some("")` =
/// direct (also ignoring proxy environment variables), otherwise that proxy.
fn source_client(config: &Config, proxy: Option<&str>, what: &str) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(&config.http.user_agent)
        .connect_timeout(Duration::from_secs(config.http.connect_timeout_secs.max(1)))
        .timeout(Duration::from_secs(config.http.timeout_secs.max(1)));
    match proxy.map(str::trim) {
        Some("") => builder = builder.no_proxy(),
        Some(proxy) => {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy).map_err(|e| Error::Config(format!("{what}: {e}")))?,
            );
        }
        None => {}
    }
    Ok(builder.build()?)
}

impl Runner {
    pub fn new(config: Config) -> Result<Self> {
        let client = source_client(&config, config.http.proxy.as_deref(), "http.proxy")?;
        let mut region_clients = BTreeMap::new();
        for (region, source) in &config.regions {
            if let Some(proxy) = source.proxy.as_deref() {
                let what = format!("regions.{region}.proxy");
                region_clients.insert(*region, source_client(&config, Some(proxy), &what)?);
            }
        }
        let publishers = Publishers::new(&config.publish, &config.http.user_agent)?;
        let state = StateStore::new(&config.state.dir);
        Ok(Self {
            config,
            client,
            region_clients,
            publishers,
            state,
        })
    }

    fn client_for(&self, region: ServerRegion) -> &reqwest::Client {
        self.region_clients.get(&region).unwrap_or(&self.client)
    }

    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    fn source(&self, region: ServerRegion) -> Result<&RegionConfig> {
        self.config
            .regions
            .get(&region)
            .filter(|source| !source.url.trim().is_empty())
            .ok_or_else(|| Error::Config(format!("regions.{region}.url is not configured")))
    }

    fn headers(source: &RegionConfig) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        for (name, value) in &source.headers {
            let name = HeaderName::try_from(name.as_str())
                .map_err(|e| Error::Config(format!("header {name:?}: {e}")))?;
            let value = HeaderValue::try_from(value.as_str())
                .map_err(|e| Error::Config(format!("header {name}: {e}")))?;
            headers.insert(name, value);
        }
        Ok(headers)
    }

    /// Probes a remote package with the client (and proxy) of `region`.
    pub async fn probe(
        &self,
        region: ServerRegion,
        url: &str,
        headers: &HeaderMap,
    ) -> Result<RemoteInfo> {
        remote::probe(self.client_for(region), url, headers).await
    }

    /// Downloads (by ranges) and parses a probed remote package.
    pub async fn extract_remote(
        &self,
        region: ServerRegion,
        platform: &str,
        info: &RemoteInfo,
        headers: HeaderMap,
    ) -> Result<AppIdentity> {
        let client = self.client_for(region).clone();
        let handle = Handle::current();
        let page = self.config.http.page_size_kib.max(4) * 1024;
        let cache = self.config.http.cache_mib.max(1) * 1024 * 1024;
        let probe = info.clone();
        let started = Instant::now();
        let (scan, stats) = tokio::task::spawn_blocking(move || {
            let (region, stats) =
                remote::open_region(client, handle, &probe, headers, page, cache)?;
            scan_package(region).map(|scan| (scan, stats))
        })
        .await
        .map_err(|e| Error::Other(format!("extraction task failed: {e}")))??;
        info!(
            %region,
            elapsed_ms = started.elapsed().as_millis() as u64,
            requests = stats.requests(),
            bytes = stats.bytes(),
            "Extracted package"
        );
        AppIdentity::from_scan(
            region,
            platform,
            scan,
            Some(SourceInfo::from_remote(info, Some(stats.bytes()))),
        )
    }

    /// Parses a local package file.
    pub async fn extract_file(
        region: ServerRegion,
        platform: &str,
        path: &Path,
    ) -> Result<AppIdentity> {
        let owned = path.to_path_buf();
        let scan = tokio::task::spawn_blocking(move || {
            let region = Region::from_file(&owned)?;
            scan_package(region)
        })
        .await
        .map_err(|e| Error::Other(format!("extraction task failed: {e}")))??;
        let size = tokio::fs::metadata(path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        AppIdentity::from_scan(
            region,
            platform,
            scan,
            Some(SourceInfo {
                location: path.display().to_string(),
                size,
                etag: None,
                last_modified: None,
                file_name: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                bytes_fetched: None,
            }),
        )
    }

    /// Extracts `region` from its configured source, ignoring state.
    pub async fn extract(&self, region: ServerRegion) -> Result<AppIdentity> {
        let source = self.source(region)?;
        let headers = Self::headers(source)?;
        let info = self.probe(region, &source.url, &headers).await?;
        self.extract_remote(region, &source.platform, &info, headers)
            .await
    }

    /// Checks one region: reuse the cached identity when the package is
    /// unchanged, extract otherwise, then publish.
    pub async fn check(&self, region: ServerRegion, options: RunOptions) -> RegionReport {
        let timeout = Duration::from_secs(self.config.schedule.region_timeout_secs.max(1));
        let mut report = RegionReport {
            region,
            extracted: false,
            identity: None,
            error: None,
            publish_errors: Vec::new(),
            checked_at: Utc::now(),
        };
        match tokio::time::timeout(timeout, self.check_inner(region, options, &mut report)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => report.error = Some(error.to_string()),
            Err(_) => report.error = Some(format!("timed out after {}s", timeout.as_secs())),
        }
        if let Some(error) = &report.error {
            warn!(%region, %error, "Region check failed");
        }
        report
    }

    async fn check_inner(
        &self,
        region: ServerRegion,
        options: RunOptions,
        report: &mut RegionReport,
    ) -> Result<()> {
        let source = self.source(region)?;
        let headers = Self::headers(source)?;
        let mut state = self.state.load(region).await;
        let info = self.probe(region, &source.url, &headers).await?;
        let fingerprint = info.fingerprint();
        let identity = match state.identity.clone() {
            Some(cached) if !options.force && state.fingerprint == fingerprint => {
                info!(%region, app_version = %cached.app_version, "Package unchanged; reusing cached identity");
                cached
            }
            _ => {
                let identity = self
                    .extract_remote(region, &source.platform, &info, headers)
                    .await?;
                report.extracted = true;
                if state.identity.as_ref().map(AppIdentity::essence) != Some(identity.essence()) {
                    info!(
                        %region,
                        app_version = %identity.app_version,
                        app_hash = %identity.app_hash,
                        previous = ?state.identity.as_ref().map(|i| i.app_version.clone()),
                        "New app identity"
                    );
                }
                state.fingerprint = fingerprint;
                state.identity = Some(identity.clone());
                identity
            }
        };
        if options.publish && !self.publishers.is_empty() {
            let failures = self
                .publishers
                .publish(&identity, &mut state.delivered, options.force)
                .await;
            report.publish_errors = failures
                .into_iter()
                .map(|(name, error)| format!("{name}: {error}"))
                .collect();
        }
        if let Err(error) = self.state.save(region, &state).await {
            warn!(%region, %error, "Cannot save state");
        }
        report.identity = Some(identity);
        Ok(())
    }

    /// Checks `regions` one after another.
    pub async fn run_once(
        &self,
        regions: &[ServerRegion],
        options: RunOptions,
    ) -> Vec<RegionReport> {
        let mut reports = Vec::with_capacity(regions.len());
        for &region in regions {
            reports.push(self.check(region, options).await);
        }
        reports
    }
}

/// Shared handle for the service mode.
pub type SharedRunner = Arc<Runner>;
