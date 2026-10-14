//! End-to-end tests over in-process HTTP servers: range extraction, caching,
//! and every publisher. No network access.

mod support;

use std::collections::BTreeMap;

use axum::http::StatusCode;
use sekai_apphash::config::{
    Config, FilePublisherConfig, HarukiPublisherConfig, RegionConfig, Secret, StateConfig,
    WebhookConfig,
};
use sekai_apphash::publish::{sign, SIGNATURE_HEADER};
use sekai_apphash::runner::{RunOptions, Runner};
use sekai_apphash::testing::{xapk, xapk_with_manifest, Fixture};
use sekai_apphash::ServerRegion;
use support::{HookReceiver, MockNode, PackageServer};

const TW_HASH: &str = "d67a688b-ff78-4300-bfc5-2120c000d75d";
const TOKEN: &str = "test-internal-token";

fn config_for(url: String) -> Config {
    let mut config = Config {
        state: StateConfig {
            dir: std::path::PathBuf::new(),
        },
        ..Config::default()
    };
    config.regions.insert(
        ServerRegion::Tw,
        RegionConfig {
            url,
            ..RegionConfig::default()
        },
    );
    config
}

#[tokio::test]
async fn extracts_through_redirects_with_range_requests() {
    let fixture = xapk("com.hermes.mk.asia", ("6", "4", "1"), TW_HASH);
    let total = fixture.bytes.len();
    let server = PackageServer::new(fixture.bytes);
    let addr = server.start().await;
    let runner = Runner::new(config_for(format!("http://{addr}/latest"))).unwrap();

    let identity = runner.extract(ServerRegion::Tw).await.unwrap();
    // TW logs in with the manifest version; the config's version is kept.
    assert_eq!(identity.app_version, "6.4.0");
    assert_eq!(identity.client_version, "6.4.1");
    assert_eq!(identity.app_hash, TW_HASH);
    assert_eq!(identity.ab_version, "6.4.0");
    assert_eq!(identity.config, "production_android");
    assert_eq!(identity.configs.len(), 2);
    let manifest = identity.manifest.as_ref().unwrap();
    assert_eq!(manifest.version_name.as_deref(), Some("6.4.0"));
    let source = identity.source.as_ref().unwrap();
    assert_eq!(source.size, total as u64);
    assert_eq!(source.etag.as_deref(), Some("\"fixture-v1\""));
    assert_eq!(source.file_name.as_deref(), Some("Fixture_6.4.0.xapk"));
    assert!(source.bytes_fetched.unwrap() <= total as u64);
    assert!(server.requests.load(std::sync::atomic::Ordering::SeqCst) > 1);

    // The package is TW's: asking for KR must fail instead of mislabelling it.
    let mut config = config_for(String::new());
    config.regions.insert(
        ServerRegion::Kr,
        RegionConfig {
            url: format!("http://{addr}/pkg"),
            ..RegionConfig::default()
        },
    );
    let runner = Runner::new(config).unwrap();
    let error = runner.extract(ServerRegion::Kr).await.unwrap_err();
    assert!(error.to_string().contains("belongs to tw"), "{error}");
    let error = runner.extract(ServerRegion::Tw).await.unwrap_err();
    assert!(error.to_string().contains("no enabled source"), "{error}");
}

#[tokio::test]
async fn sources_without_range_support_are_rejected() {
    let server = PackageServer::new(vec![0; 100]);
    let addr = server.start().await;
    let runner = Runner::new(config_for(format!("http://{addr}/plain"))).unwrap();
    let error = runner.extract(ServerRegion::Tw).await.unwrap_err();
    assert!(error.to_string().contains("byte ranges"), "{error}");
}

struct Harness {
    node: MockNode,
    hooks: HookReceiver,
    out: tempfile::TempDir,
    config: Config,
}

async fn harness(node_token: &str) -> Harness {
    harness_with(
        node_token,
        xapk("com.hermes.mk.asia", ("6", "4", "1"), TW_HASH),
    )
    .await
}

async fn harness_with(node_token: &str, fixture: Fixture) -> Harness {
    let server = PackageServer::new(fixture.bytes);
    let addr = server.start().await;
    let node = MockNode::new(node_token);
    let node_addr = node.start().await;
    let hooks = HookReceiver::default();
    let hook_addr = hooks.start().await;
    let out = tempfile::tempdir().unwrap();

    let mut config = config_for(format!("http://{addr}/latest"));
    config.state.dir = out.path().join("state");
    config.publish.haruki = Some(HarukiPublisherConfig {
        base_url: format!("http://{node_addr}/"),
        token: Secret::new(TOKEN),
        ..HarukiPublisherConfig::default()
    });
    config.publish.file = Some(FilePublisherConfig {
        path: Some(out.path().join("apphash.json")),
        dir: Some(out.path().join("regions")),
        ..FilePublisherConfig::default()
    });
    config.publish.webhooks = BTreeMap::from([(
        "audit".to_owned(),
        WebhookConfig {
            url: format!("http://{hook_addr}/hook"),
            hmac_secret: Secret::new("hook-secret"),
            bearer_token: Secret::new("hook-bearer"),
            ..WebhookConfig::default()
        },
    )]);
    config.validate().unwrap();
    Harness {
        node,
        hooks,
        out,
        config,
    }
}

const PUBLISH: RunOptions = RunOptions {
    force: false,
    publish: true,
};

#[tokio::test]
async fn a_run_publishes_once_and_then_reuses_the_cache() {
    let h = harness(TOKEN).await;
    h.node.set("tw", "6.3.0", "old-hash");
    let runner = Runner::new(h.config.clone()).unwrap();

    let first = runner.run_once(&[ServerRegion::Tw], PUBLISH).await;
    assert!(first[0].is_ok(), "{:?}", first[0]);
    assert!(first[0].extracted);
    let posts = h.node.posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["server"], "tw");
    assert_eq!(posts[0]["appVersion"], "6.4.0");
    assert_eq!(posts[0]["appHash"], TW_HASH);
    assert_eq!(
        h.node.versions.lock().unwrap()["tw"],
        ("6.4.0".to_owned(), TW_HASH.to_owned())
    );

    let combined: serde_json::Value =
        serde_json::from_slice(&std::fs::read(h.out.path().join("apphash.json")).unwrap()).unwrap();
    assert_eq!(combined["tw"]["appHash"], TW_HASH);
    let single: serde_json::Value =
        serde_json::from_slice(&std::fs::read(h.out.path().join("regions/tw.json")).unwrap())
            .unwrap();
    assert_eq!(single["appVersion"], "6.4.0");
    assert_eq!(single["appVersionSource"], "manifest");
    assert_eq!(single["clientVersion"], "6.4.1");

    {
        let deliveries = h.hooks.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        let (headers, body) = &deliveries[0];
        assert_eq!(
            headers[SIGNATURE_HEADER].to_str().unwrap(),
            sign("hook-secret", body)
        );
        assert_eq!(headers["authorization"], "Bearer hook-bearer");
        let payload: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(payload["event"], "app-identity");
        assert_eq!(payload["identity"]["appHash"], TW_HASH);
    }

    // Same package: no download, no POST, no webhook.
    let second = runner.run_once(&[ServerRegion::Tw], PUBLISH).await;
    assert!(second[0].is_ok(), "{:?}", second[0]);
    assert!(!second[0].extracted);
    assert_eq!(h.node.posts.lock().unwrap().len(), 1);
    assert_eq!(h.hooks.deliveries.lock().unwrap().len(), 1);

    // A fresh runner on the same state directory also reuses the cache.
    let again = Runner::new(h.config.clone()).unwrap();
    let third = again.run_once(&[ServerRegion::Tw], PUBLISH).await;
    assert!(!third[0].extracted);

    // --force re-extracts and re-delivers everywhere.
    let forced = again
        .run_once(
            &[ServerRegion::Tw],
            RunOptions {
                force: true,
                publish: true,
            },
        )
        .await;
    assert!(forced[0].is_ok() && forced[0].extracted);
    assert_eq!(h.node.posts.lock().unwrap().len(), 2);
    assert_eq!(h.hooks.deliveries.lock().unwrap().len(), 2);

    // Every node call carried the token; it never shows up in reports.
    assert!(h
        .node
        .auth_headers
        .lock()
        .unwrap()
        .iter()
        .all(|v| v == &format!("Bearer {TOKEN}")));
}

#[tokio::test]
async fn downgrades_are_refused() {
    let h = harness(TOKEN).await;
    h.node.set("tw", "6.5.0", "newer-hash");
    let runner = Runner::new(h.config.clone()).unwrap();
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.error.is_none());
    assert_eq!(report.publish_errors.len(), 1);
    assert!(report.publish_errors[0].contains("refusing downgrade 6.5.0 -> 6.4.0"));
    assert!(h.node.posts.lock().unwrap().is_empty());
    // The other publishers still ran.
    assert_eq!(h.hooks.deliveries.lock().unwrap().len(), 1);

    let mut config = h.config.clone();
    config.publish.haruki.as_mut().unwrap().refuse_downgrade = false;
    let runner = Runner::new(config).unwrap();
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(h.node.posts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_nuverse_patch_release_is_published_with_its_manifest_version() {
    let fixture = xapk_with_manifest(
        "com.hermes.mk.asia",
        ("6", "4", "2"),
        Some("6.4.1"),
        TW_HASH,
    );
    let h = harness_with(TOKEN, fixture).await;
    h.node
        .keep_patch
        .store(true, std::sync::atomic::Ordering::SeqCst);
    h.node.set("tw", "6.4.0", "old-hash");
    let runner = Runner::new(h.config.clone()).unwrap();
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.is_ok(), "{report:?}");
    let identity = report.identity.unwrap();
    assert_eq!(identity.app_version, "6.4.1");
    assert_eq!(identity.client_version, "6.4.2");
    let posts = h.node.posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["appVersion"], "6.4.1");
    assert_eq!(
        h.node.versions.lock().unwrap()["tw"],
        ("6.4.1".to_owned(), TW_HASH.to_owned())
    );

    // The node now matches: nothing more is sent.
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(h.node.posts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_patch_level_downgrade_is_refused() {
    let h = harness(TOKEN).await;
    // The node holds a patch release; the package's manifest says 6.4.0.
    h.node.set("tw", "6.4.1", "newer-hash");
    let error = haruki_failure(&h.config).await;
    assert!(
        error.contains("refusing downgrade 6.4.1 -> 6.4.0"),
        "{error}"
    );
    assert!(h.node.posts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_package_without_a_manifest_version_falls_back_to_the_zeroed_config_version() {
    let fixture = xapk_with_manifest("com.hermes.mk.asia", ("6", "4", "1"), None, TW_HASH);
    let h = harness_with(TOKEN, fixture).await;
    h.node.set("tw", "6.3.0", "old-hash");
    let runner = Runner::new(h.config.clone()).unwrap();
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.is_ok(), "{report:?}");
    let identity = report.identity.unwrap();
    assert_eq!(identity.app_version, "6.4.0");
    assert_eq!(identity.client_version, "6.4.1");
    let posts = h.node.posts.lock().unwrap().clone();
    assert_eq!(posts[0]["appVersion"], "6.4.0");
}

#[tokio::test]
async fn state_written_by_0_2_is_not_pushed_again() {
    let h = harness(TOKEN).await;
    h.node.set("tw", "6.3.0", "old-hash");
    let runner = Runner::new(h.config.clone()).unwrap();
    assert!(runner.run_once(&[ServerRegion::Tw], PUBLISH).await[0].is_ok());
    assert_eq!(h.node.posts.lock().unwrap().len(), 1);

    // Rewrite the cached identity the way 0.2.0 stored it: the config
    // version as appVersion and none of the newer fields.
    let path = h.config.state.dir.join("tw.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let identity = state["identity"].as_object_mut().unwrap();
    identity.insert("appVersion".into(), "6.4.1".into());
    identity.remove("appVersionSource");
    identity.remove("clientVersion");
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();

    // The node holds the normalized 6.4.0, as 0.2.0 left it. Neither a
    // second push nor a downgrade refusal: the region is unchanged.
    assert_eq!(
        h.node.versions.lock().unwrap()["tw"],
        ("6.4.0".to_owned(), TW_HASH.to_owned())
    );
    let runner = Runner::new(h.config.clone()).unwrap();
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert!(!report.extracted);
    assert_eq!(report.identity.unwrap().app_version, "6.4.0");
    assert_eq!(h.node.posts.lock().unwrap().len(), 1);
}

/// Runs one publishing pass for TW and returns the Haruki publisher's error.
async fn haruki_failure(config: &Config) -> String {
    let runner = Runner::new(config.clone()).unwrap();
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.error.is_none(), "{report:?}");
    assert!(!report.is_ok(), "{report:?}");
    assert_eq!(report.publish_errors.len(), 1, "{report:?}");
    report.publish_errors[0].clone()
}

#[tokio::test]
async fn an_unreadable_node_identity_blocks_the_push() {
    let h = harness(TOKEN).await;

    // No version file on the node: `{ok: false}` envelope.
    let error = haruki_failure(&h.config).await;
    assert!(error.starts_with("haruki"), "{error}");
    assert!(
        error.contains("cannot read the node's current identity"),
        "{error}"
    );
    assert!(error.contains("No such file"), "{error}");

    // A 5xx, and the bare 404 of a node with its internal endpoints disabled.
    for status in [StatusCode::SERVICE_UNAVAILABLE, StatusCode::NOT_FOUND] {
        *h.node.version_status.lock().unwrap() = Some(status);
        let error = haruki_failure(&h.config).await;
        assert!(error.contains(&format!("HTTP {status}")), "{error}");
    }

    // Bootstrapping does not cover an unreadable node either.
    let mut config = h.config.clone();
    config.publish.haruki.as_mut().unwrap().bootstrap_regions = vec![ServerRegion::Tw];
    let error = haruki_failure(&config).await;
    assert!(error.contains("cannot read"), "{error}");
    assert!(h.node.posts.lock().unwrap().is_empty());

    // Nothing listening at all.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_addr = closed.local_addr().unwrap();
    drop(closed);
    config.publish.haruki.as_mut().unwrap().base_url = format!("http://{closed_addr}");
    let error = haruki_failure(&config).await;
    assert!(error.contains("cannot read"), "{error}");

    // The other publishers still ran on the first pass.
    assert_eq!(h.hooks.deliveries.lock().unwrap().len(), 1);

    // Once the node answers, the push goes through.
    *h.node.version_status.lock().unwrap() = None;
    h.node.set("tw", "6.3.0", "old-hash");
    let runner = Runner::new(h.config.clone()).unwrap();
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(h.node.posts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_empty_node_identity_needs_an_explicit_bootstrap() {
    let h = harness(TOKEN).await;
    h.node.set("tw", "", "");
    let error = haruki_failure(&h.config).await;
    assert!(error.contains("bootstrap_regions"), "{error}");
    assert!(h.node.posts.lock().unwrap().is_empty());

    // Neither does turning the downgrade guard off, nor listing another region.
    let mut config = h.config.clone();
    config.publish.haruki.as_mut().unwrap().refuse_downgrade = false;
    let error = haruki_failure(&config).await;
    assert!(error.contains("bootstrap_regions"), "{error}");
    config.publish.haruki.as_mut().unwrap().refuse_downgrade = true;
    config.publish.haruki.as_mut().unwrap().bootstrap_regions = vec![ServerRegion::Kr];
    haruki_failure(&config).await;
    assert!(h.node.posts.lock().unwrap().is_empty());

    config.publish.haruki.as_mut().unwrap().bootstrap_regions = vec![ServerRegion::Tw];
    let runner = Runner::new(config).unwrap();
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(h.node.posts.lock().unwrap().len(), 1);
    assert_eq!(
        h.node.versions.lock().unwrap()["tw"],
        ("6.4.0".to_owned(), TW_HASH.to_owned())
    );

    // Bootstrapped: the normal checks apply again, even with the list kept.
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(h.node.posts.lock().unwrap().len(), 1);
    h.node.set("tw", "6.5.0", "newer-hash");
    let mut config = h.config.clone();
    config.publish.haruki.as_mut().unwrap().bootstrap_regions = vec![ServerRegion::Tw];
    let error = haruki_failure(&config).await;
    assert!(error.contains("refusing downgrade"), "{error}");
}

#[tokio::test]
async fn an_incomparable_node_version_blocks_the_push() {
    let h = harness(TOKEN).await;
    h.node.set("tw", "unknown", "some-hash");
    let error = haruki_failure(&h.config).await;
    assert!(error.contains("cannot compare"), "{error}");
    assert!(h.node.posts.lock().unwrap().is_empty());

    // Without the downgrade guard there is nothing to compare.
    let mut config = h.config.clone();
    config.publish.haruki.as_mut().unwrap().refuse_downgrade = false;
    let runner = Runner::new(config).unwrap();
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(h.node.posts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_cli_run_fails_when_the_node_identity_is_unreadable() {
    let h = harness(TOKEN).await;
    let haruki = h.config.publish.haruki.as_ref().unwrap();
    let config = format!(
        "[state]\ndir = {state:?}\n\n[regions.tw]\nurl = {url:?}\n\n\
         [publish.haruki]\nbase_url = {base:?}\n",
        state = h.out.path().join("cli-state"),
        url = h.config.regions[&ServerRegion::Tw].url,
        base = haruki.base_url,
    );
    let path = h.out.path().join("config.toml");
    std::fs::write(&path, config).unwrap();
    let run = || async {
        tokio::process::Command::new(env!("CARGO_BIN_EXE_sekai-apphash"))
            .args(["run", "tw", "--compact", "--config"])
            .arg(&path)
            .env("SEKAI_APPHASH_PUBLISH__HARUKI__TOKEN", TOKEN)
            .output()
            .await
            .unwrap()
    };

    let failed = run().await;
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("cannot read the node's current identity"),
        "{stderr}"
    );
    assert!(!stderr.contains(TOKEN));
    assert!(h.node.posts.lock().unwrap().is_empty());

    h.node.set("tw", "6.3.0", "old-hash");
    let ok = run().await;
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert_eq!(h.node.posts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_rejected_token_is_reported_without_leaking_it() {
    let h = harness("the-node-expects-another-token").await;
    let runner = Runner::new(h.config.clone()).unwrap();
    let report = runner
        .run_once(&[ServerRegion::Tw], PUBLISH)
        .await
        .remove(0);
    let errors = report.publish_errors.join("\n");
    assert!(errors.contains("401"), "{errors}");
    assert!(!errors.contains(TOKEN));
    assert!(!format!("{:?}", h.config).contains(TOKEN));

    // Publishing is off: nothing is delivered anywhere.
    let quiet = runner
        .run_once(
            &[ServerRegion::Tw],
            RunOptions {
                force: true,
                publish: false,
            },
        )
        .await
        .remove(0);
    assert!(quiet.is_ok());
    assert_eq!(h.hooks.deliveries.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_cli_extracts_a_local_package() {
    let fixture = xapk("com.pjsekai.kr", ("6", "4", "1"), TW_HASH);
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("kr.xapk");
    std::fs::write(&package, &fixture.bytes).unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_sekai-apphash"))
        .args(["extract", "kr", "--brief", "--file"])
        .arg(&package)
        .env_remove("SEKAI_APPHASH_CONFIG")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["identities"]["kr"]["appHash"], TW_HASH);
    assert!(json["identities"]["kr"].get("configs").is_none());

    let failed = tokio::process::Command::new(env!("CARGO_BIN_EXE_sekai-apphash"))
        .args(["extract", "jp", "--file"])
        .arg(&package)
        .output()
        .await
        .unwrap();
    assert!(!failed.status.success());
    let json: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert!(json["errors"]["jp"]
        .as_str()
        .unwrap()
        .contains("belongs to kr"));
}

#[tokio::test]
async fn a_region_proxy_overrides_the_global_one() {
    let fixture = xapk("com.hermes.mk.asia", ("6", "4", "1"), TW_HASH);
    let server = PackageServer::new(fixture.bytes);
    let addr = server.start().await;
    // Nothing listens on port 9: every request through this proxy fails.
    let dead_proxy = "http://127.0.0.1:9".to_owned();

    let mut config = config_for(format!("http://{addr}/pkg"));
    config.http.proxy = Some(dead_proxy.clone());
    config.http.connect_timeout_secs = 2;
    let runner = Runner::new(config.clone()).unwrap();
    assert!(runner.extract(ServerRegion::Tw).await.is_err());

    config.regions.get_mut(&ServerRegion::Tw).unwrap().proxy = Some(String::new());
    let runner = Runner::new(config.clone()).unwrap();
    let identity = runner.extract(ServerRegion::Tw).await.unwrap();
    assert_eq!(identity.app_hash, TW_HASH);

    config.http.proxy = None;
    config.regions.get_mut(&ServerRegion::Tw).unwrap().proxy = Some(dead_proxy);
    let runner = Runner::new(config).unwrap();
    assert!(runner.extract(ServerRegion::Tw).await.is_err());
}
