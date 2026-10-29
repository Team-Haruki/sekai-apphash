//! One pass over the configured regions: probe, extract when the package
//! changed, publish.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use tokio::runtime::Handle;
use tracing::{debug, info, warn};
use unity_rs_core::source::Region;

use crate::config::{Config, RegionConfig, SourceConfig, SourceKind};
use crate::error::{Error, Result};
use crate::extract::scan_package;
use crate::identity::{AppIdentity, SourceInfo};
use crate::publish::Publishers;
use crate::qooapp;
use crate::remote::{self, RemoteInfo};
use crate::server::ServerRegion;
use crate::state::{RegionState, StateStore};

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

/// The waits before re-trying regions whose sources all failed transiently,
/// shared by every region of one pass. An outage that hits all regions is
/// waited out once (by the first region); when it outlasts the schedule, the
/// remaining regions fail at once instead of each waiting again.
#[derive(Debug)]
pub struct RetryBudget {
    delays: Vec<Duration>,
    used: AtomicUsize,
}

impl RetryBudget {
    #[must_use]
    pub fn new(delays_secs: &[u64]) -> Self {
        Self {
            delays: delays_secs
                .iter()
                .map(|s| Duration::from_secs(*s))
                .collect(),
            used: AtomicUsize::new(0),
        }
    }

    /// The next retry (1-based) and its wait, or `None` when spent.
    pub fn next(&self) -> Option<(usize, Duration)> {
        let index = self.used.fetch_add(1, Ordering::Relaxed);
        self.delays.get(index).map(|delay| (index + 1, *delay))
    }

    /// The most a pass can wait in total.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.delays.iter().sum()
    }
}

/// Every enabled source of a region failed.
struct SourcesFailed {
    error: Error,
    /// At least one source was tried and every tried source failed with a
    /// transient error (skipped sources do not count).
    transient: bool,
}

pub struct Runner {
    config: Config,
    client: reqwest::Client,
    /// Clients of sources whose `proxy` differs from `http.proxy`, by region
    /// and source index.
    source_clients: BTreeMap<(ServerRegion, usize), reqwest::Client>,
    /// Resolved channel headers (values marked sensitive).
    channel_headers: BTreeMap<String, HeaderMap>,
    /// Channels with an empty required header -> the missing names.
    unavailable_channels: BTreeMap<String, Vec<String>>,
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

fn header_map<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    sensitive: bool,
) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::try_from(name)
            .map_err(|e| Error::Config(format!("header {name:?}: {e}")))?;
        // The value is never part of the message: it may be a credential.
        let mut value = HeaderValue::try_from(value)
            .map_err(|_| Error::Config(format!("header {name}: invalid value")))?;
        value.set_sensitive(sensitive);
        map.insert(name, value);
    }
    Ok(map)
}

/// A source resolved to its current package.
struct Resolved {
    /// Change detection key, compared with the region state.
    fingerprint: String,
    package: Package,
}

enum Package {
    /// Already probed (a direct source probes to get its fingerprint).
    Probed(RemoteInfo),
    /// Probed only when the package must be read; `display` is the URL
    /// without its query, for reports.
    Url { url: String, display: String },
}

impl Runner {
    pub fn new(config: Config) -> Result<Self> {
        let client = source_client(&config, config.http.proxy.as_deref(), "http.proxy")?;
        let mut source_clients = BTreeMap::new();
        for (region, region_config) in &config.regions {
            for (index, source) in region_config.effective_sources().iter().enumerate() {
                if let Some(proxy) = source.proxy.as_deref() {
                    let what = format!("regions.{region}.sources[{index}].proxy");
                    source_clients.insert(
                        (*region, index),
                        source_client(&config, Some(proxy), &what)?,
                    );
                }
            }
        }
        let mut channel_headers = BTreeMap::new();
        let mut unavailable_channels = BTreeMap::new();
        for (name, channel) in &config.channels {
            let headers = channel.resolved_headers()?;
            channel_headers.insert(
                name.clone(),
                header_map(headers.iter().map(|(n, v)| (n.as_str(), v.expose())), true)
                    .map_err(|e| Error::Config(format!("channels.{name}: {e}")))?,
            );
            let missing = channel.missing_required();
            if !missing.is_empty() {
                unavailable_channels.insert(name.clone(), missing);
            }
        }
        let runner = Self {
            publishers: Publishers::new(&config.publish, &config.http.user_agent)?,
            state: StateStore::new(&config.state.dir),
            config,
            client,
            source_clients,
            channel_headers,
            unavailable_channels,
        };
        runner.warn_unavailable_channels();
        Ok(runner)
    }

    /// One warning per channel whose required headers are empty, naming the
    /// enabled sources that will be skipped (never the header values).
    fn warn_unavailable_channels(&self) {
        for (channel, missing) in &self.unavailable_channels {
            let sources: Vec<String> = self
                .config
                .enabled_regions()
                .into_iter()
                .flat_map(|region| {
                    self.config.regions[&region]
                        .effective_sources()
                        .into_iter()
                        .filter(|s| s.enabled && &s.channel == channel)
                        .map(move |s| format!("{region}/{}", s.label()))
                })
                .collect();
            if !sources.is_empty() {
                warn!(
                    %channel,
                    missing = %missing.join(","),
                    sources = %sources.join(","),
                    "Channel has empty required headers; its sources are skipped until they are set"
                );
            }
        }
    }

    fn client_for(&self, region: ServerRegion, index: usize) -> &reqwest::Client {
        self.source_clients
            .get(&(region, index))
            .unwrap_or(&self.client)
    }

    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    fn region_config(&self, region: ServerRegion) -> Result<&RegionConfig> {
        self.config
            .regions
            .get(&region)
            .filter(|config| config.has_enabled_source())
            .ok_or_else(|| Error::Config(format!("regions.{region} has no enabled source")))
    }

    /// The channel's headers, then the source's own.
    fn headers(&self, source: &SourceConfig) -> Result<HeaderMap> {
        let mut headers = if source.channel.is_empty() {
            HeaderMap::new()
        } else {
            self.channel_headers
                .get(&source.channel)
                .cloned()
                .ok_or_else(|| {
                    Error::Config(format!("channel {:?} is not defined", source.channel))
                })?
        };
        let own = header_map(
            source.headers.iter().map(|(n, v)| (n.as_str(), v.as_str())),
            false,
        )?;
        for (name, value) in own {
            if let Some(name) = name {
                headers.insert(name, value);
            }
        }
        Ok(headers)
    }

    /// Probes a remote package with the client (and proxy) of the region's
    /// first source.
    pub async fn probe(
        &self,
        region: ServerRegion,
        url: &str,
        headers: &HeaderMap,
    ) -> Result<RemoteInfo> {
        remote::probe(self.client_for(region, 0), url, headers).await
    }

    /// Downloads (by ranges) and parses a probed remote package, accepting
    /// only the region's own package name.
    pub async fn extract_remote(
        &self,
        region: ServerRegion,
        platform: &str,
        info: &RemoteInfo,
        headers: HeaderMap,
    ) -> Result<AppIdentity> {
        let client = self.client_for(region, 0).clone();
        self.extract_with(client, region, platform, info, headers, &[])
            .await
    }

    async fn extract_with(
        &self,
        client: reqwest::Client,
        region: ServerRegion,
        platform: &str,
        info: &RemoteInfo,
        headers: HeaderMap,
        packages: &[String],
    ) -> Result<AppIdentity> {
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
            packages,
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
            &[],
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

    /// Extracts `region` from the first of its sources that works, ignoring
    /// state.
    pub async fn extract(&self, region: ServerRegion) -> Result<AppIdentity> {
        let mut extracted = false;
        let retry = self.retry_budget();
        self.obtain(region, true, None, &mut extracted, &retry)
            .await
    }

    /// A fresh retry schedule for one pass.
    #[must_use]
    pub fn retry_budget(&self) -> RetryBudget {
        RetryBudget::new(&self.config.schedule.retry_delays_secs)
    }

    fn lookup_timeout(&self) -> Duration {
        Duration::from_secs(
            self.config
                .http
                .lookup_timeout_secs
                .clamp(1, self.config.http.timeout_secs.max(1)),
        )
    }

    /// Finds out what `source` currently serves.
    async fn resolve(
        &self,
        region: ServerRegion,
        index: usize,
        source: &SourceConfig,
        headers: &HeaderMap,
    ) -> Result<Resolved> {
        let client = self.client_for(region, index);
        match source.kind {
            SourceKind::Direct => {
                let info = remote::probe_within(
                    client,
                    &source.download_url,
                    headers,
                    self.lookup_timeout(),
                )
                .await?;
                Ok(Resolved {
                    fingerprint: info.fingerprint(),
                    package: Package::Probed(info),
                })
            }
            SourceKind::Qooapp => {
                let release = qooapp::resolve(
                    client,
                    &source.check_url,
                    &source.download_url,
                    headers,
                    self.lookup_timeout(),
                )
                .await?;
                debug!(
                    %region,
                    source = source.label(),
                    package_id = %release.package_id,
                    version = ?release.version_name,
                    asset_pack = release.asset_pack,
                    "QooApp store answered"
                );
                Ok(Resolved {
                    fingerprint: release.fingerprint(),
                    package: Package::Url {
                        url: release.download_url,
                        display: release.display_url,
                    },
                })
            }
        }
    }

    /// Tries the region's sources in order and returns the first identity.
    /// With `state`, an unchanged package reuses the cached identity and a
    /// new one is recorded there; `force` re-extracts regardless. When every
    /// source failed transiently, waits and tries them all again while
    /// `retry` allows. Only this lookup repeats; nothing is published here.
    async fn obtain(
        &self,
        region: ServerRegion,
        force: bool,
        mut state: Option<&mut RegionState>,
        extracted: &mut bool,
        retry: &RetryBudget,
    ) -> Result<AppIdentity> {
        loop {
            let failed = match self
                .obtain_once(region, force, state.as_deref_mut(), extracted)
                .await
            {
                Ok(identity) => return Ok(identity),
                Err(failed) => failed,
            };
            if !failed.transient {
                return Err(failed.error);
            }
            let Some((attempt, delay)) = retry.next() else {
                return Err(failed.error);
            };
            warn!(
                %region,
                retry = attempt,
                delay_secs = delay.as_secs(),
                error = %failed.error,
                "Every source failed with a connection error; retrying the region"
            );
            tokio::time::sleep(delay).await;
        }
    }

    /// One try of the region's sources in order.
    async fn obtain_once(
        &self,
        region: ServerRegion,
        force: bool,
        mut state: Option<&mut RegionState>,
        extracted: &mut bool,
    ) -> Result<AppIdentity, SourcesFailed> {
        let region_config = self.region_config(region).map_err(|error| SourcesFailed {
            error,
            transient: false,
        })?;
        let mut failures = Vec::new();
        let mut tried = 0usize;
        let mut transient = 0usize;
        for (index, source) in region_config.effective_sources().iter().enumerate() {
            if !source.enabled {
                continue;
            }
            if let Some(missing) = self.unavailable_channels.get(&source.channel) {
                // Warned once at startup; no request is sent.
                debug!(%region, source = source.label(), "Skipping source: channel has empty required headers");
                failures.push(format!(
                    "{}: skipped (channel {} lacks {})",
                    source.label(),
                    source.channel,
                    missing.join(",")
                ));
                continue;
            }
            tried += 1;
            let attempt = self
                .obtain_from(
                    region,
                    index,
                    source,
                    &region_config.platform,
                    force,
                    state.as_deref_mut(),
                    extracted,
                )
                .await;
            match attempt {
                Ok(identity) => {
                    if !failures.is_empty() {
                        info!(%region, source = source.label(), "Used a fallback source");
                    }
                    return Ok(identity);
                }
                Err(error) => {
                    warn!(%region, source = source.label(), %error, "Source failed");
                    if error.is_transient() {
                        transient += 1;
                    }
                    failures.push(format!("{}: {error}", source.label()));
                }
            }
        }
        Err(SourcesFailed {
            error: Error::Other(format!(
                "no source yielded the identity ({})",
                failures.join("; ")
            )),
            transient: tried > 0 && transient == tried,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn obtain_from(
        &self,
        region: ServerRegion,
        index: usize,
        source: &SourceConfig,
        platform: &str,
        force: bool,
        state: Option<&mut RegionState>,
        extracted: &mut bool,
    ) -> Result<AppIdentity> {
        let headers = self.headers(source)?;
        let resolved = self.resolve(region, index, source, &headers).await?;
        let cached = state
            .as_deref()
            .filter(|s| !force && s.fingerprint == resolved.fingerprint)
            .and_then(|s| s.identity.clone());
        if let Some(cached) = cached {
            info!(%region, source = source.label(), app_version = %cached.app_version, client_version = %cached.client_version, "Package unchanged; reusing cached identity");
            return Ok(cached);
        }
        let info = match resolved.package {
            Package::Probed(info) => info,
            Package::Url { url, display } => {
                let mut info = remote::probe_within(
                    self.client_for(region, index),
                    &url,
                    &headers,
                    self.lookup_timeout(),
                )
                .await?;
                // Reports show the URL without its (possibly signed) query.
                info.url = display;
                info
            }
        };
        let client = self.client_for(region, index).clone();
        let identity = self
            .extract_with(client, region, platform, &info, headers, &source.packages)
            .await?;
        *extracted = true;
        if let Some(state) = state {
            if state.identity.as_ref().map(AppIdentity::essence) != Some(identity.essence()) {
                info!(
                    %region,
                    source = source.label(),
                    app_version = %identity.app_version,
                    app_version_source = ?identity.app_version_source,
                    client_version = %identity.client_version,
                    app_hash = %identity.app_hash,
                    previous = ?state.identity.as_ref().map(|i| i.app_version.clone()),
                    "New app identity"
                );
            }
            state.fingerprint = resolved.fingerprint;
            state.identity = Some(identity.clone());
        }
        Ok(identity)
    }

    /// Checks one region: reuse the cached identity when the package is
    /// unchanged, extract otherwise, then publish.
    pub async fn check(&self, region: ServerRegion, options: RunOptions) -> RegionReport {
        self.check_with(region, options, &self.retry_budget()).await
    }

    async fn check_with(
        &self,
        region: ServerRegion,
        options: RunOptions,
        retry: &RetryBudget,
    ) -> RegionReport {
        let timeout = Duration::from_secs(self.config.schedule.region_timeout_secs.max(1));
        let mut report = RegionReport {
            region,
            extracted: false,
            identity: None,
            error: None,
            publish_errors: Vec::new(),
            checked_at: Utc::now(),
        };
        match tokio::time::timeout(
            timeout,
            self.check_inner(region, options, retry, &mut report),
        )
        .await
        {
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
        retry: &RetryBudget,
        report: &mut RegionReport,
    ) -> Result<()> {
        let mut state = self.state.load(region).await;
        let identity = self
            .obtain(
                region,
                options.force,
                Some(&mut state),
                &mut report.extracted,
                retry,
            )
            .await?;
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

    /// Checks `regions` one after another, sharing one retry schedule.
    pub async fn run_once(
        &self,
        regions: &[ServerRegion],
        options: RunOptions,
    ) -> Vec<RegionReport> {
        let retry = self.retry_budget();
        let mut reports = Vec::with_capacity(regions.len());
        for &region in regions {
            reports.push(self.check_with(region, options, &retry).await);
        }
        reports
    }
}

/// Shared handle for the service mode.
pub type SharedRunner = Arc<Runner>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retry_schedule_is_shared_and_bounded() {
        let budget = RetryBudget::new(&Config::default().schedule.retry_delays_secs);
        assert_eq!(budget.total(), Duration::from_secs(135));
        assert_eq!(budget.next(), Some((1, Duration::from_secs(30))));
        assert_eq!(budget.next(), Some((2, Duration::from_secs(45))));
        assert_eq!(budget.next(), Some((3, Duration::from_secs(60))));
        assert_eq!(budget.next(), None);
        assert_eq!(budget.next(), None);
        assert_eq!(RetryBudget::new(&[]).next(), None);
    }
}
