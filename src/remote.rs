//! Random access to a remote file through HTTP range requests.
//!
//! [`PagedSource`] implements `unity-rs`'s [`Source`] trait on top of any
//! [`RangeFetcher`], so the zip reader and the Unity parsers can address a
//! remote package as if it were local while only the pages they touch are
//! downloaded. [`HttpFetcher`] is the HTTP implementation; it drives the async
//! `reqwest` client from the blocking extraction thread through a runtime
//! handle.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT_ENCODING, CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use serde::Serialize;
use tokio::runtime::Handle;
use unity_rs_core::source::{Region, Source};

use crate::error::{Error, Result};

/// Default page granularity of the range cache.
pub const DEFAULT_PAGE_SIZE: u64 = 64 * 1024;
/// Default upper bound of cached page bytes per remote file.
pub const DEFAULT_CACHE_BYTES: u64 = 128 * 1024 * 1024;

/// Fetches an inclusive byte range of one remote object.
pub trait RangeFetcher: Send + Sync {
    fn fetch(&self, start: u64, end_inclusive: u64) -> io::Result<Vec<u8>>;
}

/// Counters for what a [`PagedSource`] actually downloaded.
#[derive(Debug, Default)]
pub struct FetchStats {
    requests: AtomicU64,
    bytes: AtomicU64,
}

impl FetchStats {
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

struct PageCache {
    pages: HashMap<u64, Arc<[u8]>>,
    order: VecDeque<u64>,
    bytes: u64,
}

/// A page-cached random-access view of a remote object of known length.
///
/// Reads are rounded out to whole pages; runs of missing pages are fetched
/// with one range request each. Pages are evicted oldest-first once the cache
/// exceeds its byte budget.
pub struct PagedSource<F> {
    fetcher: F,
    length: u64,
    page_size: u64,
    max_cache_bytes: u64,
    cache: Mutex<PageCache>,
    stats: Arc<FetchStats>,
}

impl<F: RangeFetcher> PagedSource<F> {
    pub fn new(fetcher: F, length: u64, page_size: u64, max_cache_bytes: u64) -> Self {
        Self {
            fetcher,
            length,
            page_size: page_size.max(4096),
            max_cache_bytes,
            cache: Mutex::new(PageCache {
                pages: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
            }),
            stats: Arc::new(FetchStats::default()),
        }
    }

    #[must_use]
    pub fn stats(&self) -> Arc<FetchStats> {
        Arc::clone(&self.stats)
    }

    fn page_len(&self, page: u64) -> u64 {
        let start = page * self.page_size;
        self.page_size.min(self.length - start)
    }

    fn missing_runs(&self, first: u64, last: u64) -> Vec<(u64, u64)> {
        let cache = self.cache.lock().expect("page cache poisoned");
        let mut runs: Vec<(u64, u64)> = Vec::new();
        for page in first..=last {
            if cache.pages.contains_key(&page) {
                continue;
            }
            match runs.last_mut() {
                Some((_, end)) if *end + 1 == page => *end = page,
                _ => runs.push((page, page)),
            }
        }
        runs
    }

    fn fill(&self, first: u64, last: u64) -> io::Result<Vec<Arc<[u8]>>> {
        for (run_first, run_last) in self.missing_runs(first, last) {
            let start = run_first * self.page_size;
            let end = run_last * self.page_size + self.page_len(run_last) - 1;
            let bytes = self.fetcher.fetch(start, end)?;
            let expected = end - start + 1;
            if bytes.len() as u64 != expected {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("range {start}-{end} returned {} bytes", bytes.len()),
                ));
            }
            self.stats.requests.fetch_add(1, Ordering::Relaxed);
            self.stats.bytes.fetch_add(expected, Ordering::Relaxed);
            let mut cache = self.cache.lock().expect("page cache poisoned");
            for page in run_first..=run_last {
                let offset = ((page - run_first) * self.page_size) as usize;
                let len = self.page_len(page) as usize;
                let data: Arc<[u8]> = Arc::from(&bytes[offset..offset + len]);
                if cache.pages.insert(page, data).is_none() {
                    cache.order.push_back(page);
                    cache.bytes += len as u64;
                }
            }
        }
        let mut cache = self.cache.lock().expect("page cache poisoned");
        let pages = (first..=last)
            .map(|page| {
                cache
                    .pages
                    .get(&page)
                    .cloned()
                    .ok_or_else(|| io::Error::other(format!("page {page} missing after fill")))
            })
            .collect::<io::Result<Vec<_>>>()?;
        // Evict only after the pages this read needs have been pinned above.
        while cache.bytes > self.max_cache_bytes {
            let Some(oldest) = cache.order.pop_front() else {
                break;
            };
            if let Some(data) = cache.pages.remove(&oldest) {
                cache.bytes -= data.len() as u64;
            }
        }
        Ok(pages)
    }
}

impl<F: RangeFetcher> Source for PagedSource<F> {
    fn len(&self) -> u64 {
        self.length
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        if output.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(output.len() as u64)
            .filter(|end| *end <= self.length)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "read {offset}+{} exceeds remote length {}",
                        output.len(),
                        self.length
                    ),
                )
            })?;
        let first = offset / self.page_size;
        let last = (end - 1) / self.page_size;
        let pages = self.fill(first, last)?;
        let mut written = 0usize;
        for (index, page) in pages.iter().enumerate() {
            let page_start = (first + index as u64) * self.page_size;
            let from = offset.saturating_sub(page_start) as usize;
            let to = ((end - page_start) as usize).min(page.len());
            let chunk = &page[from..to];
            output[written..written + chunk.len()].copy_from_slice(chunk);
            written += chunk.len();
        }
        debug_assert_eq!(written, output.len());
        Ok(())
    }
}

/// What a range probe learned about a remote package.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteInfo {
    /// The URL that was requested.
    pub url: String,
    /// The URL after redirects; range reads go here.
    #[serde(skip)]
    pub final_url: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
}

impl RemoteInfo {
    /// A stable fingerprint of the remote object, used to skip unchanged
    /// packages. Redirect targets of store mirrors carry per-request tokens,
    /// so only the path of the final URL takes part.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let path = self.final_url.split(['?', '#']).next().unwrap_or_default();
        format!(
            "size={};etag={};last-modified={};path={}",
            self.size,
            self.etag.as_deref().unwrap_or(""),
            self.last_modified.as_deref().unwrap_or(""),
            path
        )
    }
}

/// HTTP range fetcher bound to a resolved URL.
pub struct HttpFetcher {
    client: reqwest::Client,
    handle: Handle,
    url: String,
    headers: HeaderMap,
    retries: u32,
}

impl HttpFetcher {
    async fn fetch_async(&self, start: u64, end_inclusive: u64) -> io::Result<Vec<u8>> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let outcome = match range_request(
                &self.client,
                &self.url,
                &self.headers,
                start,
                end_inclusive,
                None,
            )
            .await
            {
                Ok(response) if response.status() == StatusCode::PARTIAL_CONTENT => {
                    match response.bytes().await {
                        Ok(body) => return Ok(body.to_vec()),
                        Err(error) => Error::from(error).to_string(),
                    }
                }
                Ok(response) => format!("HTTP {}", response.status()),
                Err(error) => error.to_string(),
            };
            if attempt > self.retries {
                return Err(io::Error::other(format!(
                    "range {start}-{end_inclusive}: {outcome}"
                )));
            }
            tracing::debug!(start, end_inclusive, attempt, %outcome, "Range request failed, retrying");
            tokio::time::sleep(Duration::from_millis(500 * u64::from(attempt))).await;
        }
    }
}

impl RangeFetcher for HttpFetcher {
    fn fetch(&self, start: u64, end_inclusive: u64) -> io::Result<Vec<u8>> {
        // Runs on the blocking extraction thread, never on a runtime worker.
        self.handle.block_on(self.fetch_async(start, end_inclusive))
    }
}

async fn range_request(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    start: u64,
    end_inclusive: u64,
    timeout: Option<Duration>,
) -> Result<reqwest::Response> {
    let mut request = client
        .get(url)
        .headers(headers.clone())
        .header(RANGE, format!("bytes={start}-{end_inclusive}"))
        .header(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }
    Ok(request.send().await?)
}

/// Resolves `url` (following redirects), checks that it serves byte ranges and
/// returns what the probe learned.
pub async fn probe(client: &reqwest::Client, url: &str, headers: &HeaderMap) -> Result<RemoteInfo> {
    probe_request(client, url, headers, None).await
}

/// [`probe`] with its own request timeout instead of the client's.
pub async fn probe_within(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    timeout: Duration,
) -> Result<RemoteInfo> {
    probe_request(client, url, headers, Some(timeout)).await
}

async fn probe_request(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
    timeout: Option<Duration>,
) -> Result<RemoteInfo> {
    let response = range_request(client, url, headers, 0, 0, timeout).await?;
    let status = response.status();
    if status != StatusCode::PARTIAL_CONTENT {
        return Err(Error::Http(format!(
            "range probe answered HTTP {status}; the source must support byte ranges"
        )));
    }
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let size = header(CONTENT_RANGE.as_str())
        .as_deref()
        .and_then(parse_content_range_total)
        .ok_or_else(|| Error::Http("range probe returned no usable Content-Range".into()))?;
    let file_name = header("content-disposition")
        .as_deref()
        .and_then(content_disposition_file_name);
    Ok(RemoteInfo {
        url: url.to_owned(),
        final_url: response.url().to_string(),
        size,
        etag: header("etag"),
        last_modified: header("last-modified"),
        file_name,
    })
}

/// Opens a remote file as a page-cached [`Region`]. Must be called from a
/// blocking thread (the returned region blocks on `handle` when read).
pub fn open_region(
    client: reqwest::Client,
    handle: Handle,
    info: &RemoteInfo,
    headers: HeaderMap,
    page_size: u64,
    max_cache_bytes: u64,
) -> Result<(Region, Arc<FetchStats>)> {
    let fetcher = HttpFetcher {
        client,
        handle,
        url: info.final_url.clone(),
        headers,
        retries: 3,
    };
    let source = PagedSource::new(fetcher, info.size, page_size, max_cache_bytes);
    let stats = source.stats();
    let region = Region::new(Arc::new(source), 0, info.size)?;
    Ok((region, stats))
}

fn parse_content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

fn content_disposition_file_name(value: &str) -> Option<String> {
    let raw = value
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("filename="))?;
    let raw = raw.trim_matches('"');
    Some(percent_decode(raw).replace('+', " "))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(&input[index + 1..index + 3], 16) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemoryFetcher {
        data: Vec<u8>,
        calls: Mutex<Vec<(u64, u64)>>,
    }

    impl RangeFetcher for MemoryFetcher {
        fn fetch(&self, start: u64, end_inclusive: u64) -> io::Result<Vec<u8>> {
            self.calls.lock().unwrap().push((start, end_inclusive));
            Ok(self.data[start as usize..=end_inclusive as usize].to_vec())
        }
    }

    fn source(len: usize, page: u64, cache: u64) -> PagedSource<MemoryFetcher> {
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        PagedSource::new(
            MemoryFetcher {
                data,
                calls: Mutex::new(Vec::new()),
            },
            len as u64,
            page,
            cache,
        )
    }

    #[test]
    fn reads_across_pages_with_one_coalesced_request() {
        let src = source(20_000, 4096, u64::MAX);
        let mut buf = vec![0u8; 9000];
        src.read_exact_at(100, &mut buf).unwrap();
        let expected: Vec<u8> = (100..9100).map(|i| (i % 251) as u8).collect();
        assert_eq!(buf, expected);
        assert_eq!(*src.fetcher.calls.lock().unwrap(), vec![(0, 12287)]);
        // Fully cached now: no more requests.
        src.read_exact_at(5000, &mut buf[..10]).unwrap();
        assert_eq!(src.fetcher.calls.lock().unwrap().len(), 1);
        assert_eq!(src.stats().requests(), 1);
        assert_eq!(src.stats().bytes(), 12288);
    }

    #[test]
    fn fetches_only_missing_runs_and_clamps_the_last_page() {
        let src = source(10_000, 4096, u64::MAX);
        let mut buf = [0u8; 10];
        src.read_exact_at(4096, &mut buf).unwrap();
        let mut all = vec![0u8; 10_000];
        src.read_exact_at(0, &mut all).unwrap();
        assert_eq!(
            *src.fetcher.calls.lock().unwrap(),
            vec![(4096, 8191), (0, 4095), (8192, 9999)]
        );
        assert_eq!(all[9999], (9999 % 251) as u8);
    }

    #[test]
    fn evicts_oldest_pages_beyond_budget() {
        let src = source(40_000, 4096, 8192);
        let mut buf = [0u8; 1];
        for offset in [0, 4096, 8192, 12288] {
            src.read_exact_at(offset, &mut buf).unwrap();
        }
        src.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(src.fetcher.calls.lock().unwrap().len(), 5);
        assert!(src.cache.lock().unwrap().bytes <= 8192);
    }

    #[test]
    fn rejects_reads_past_the_end() {
        let src = source(100, 4096, u64::MAX);
        let mut buf = [0u8; 10];
        let error = src.read_exact_at(95, &mut buf).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        src.read_exact_at(90, &mut buf).unwrap();
        src.read_exact_at(0, &mut []).unwrap();
    }

    #[test]
    fn parses_probe_headers() {
        assert_eq!(
            parse_content_range_total("bytes 0-0/476399565"),
            Some(476_399_565)
        );
        assert_eq!(parse_content_range_total("bytes */10"), Some(10));
        assert_eq!(parse_content_range_total("garbage"), None);
        assert_eq!(
            content_disposition_file_name(
                "attachment; filename=\"Example+Game%3A+Stage%21_6.0.0.xapk\""
            )
            .as_deref(),
            Some("Example Game: Stage!_6.0.0.xapk")
        );
        assert_eq!(content_disposition_file_name("inline"), None);
        assert_eq!(percent_decode("a%2"), "a%2");
    }

    #[test]
    fn fingerprint_ignores_redirect_query_tokens() {
        let mut info = RemoteInfo {
            url: "https://example.com/latest".into(),
            final_url: "https://cdn.example.com/a.apk?token=1".into(),
            size: 10,
            etag: Some("\"x\"".into()),
            last_modified: None,
            file_name: None,
        };
        let first = info.fingerprint();
        info.final_url = "https://cdn.example.com/a.apk?token=2".into();
        assert_eq!(first, info.fingerprint());
        info.size = 11;
        assert_ne!(first, info.fingerprint());
    }
}
