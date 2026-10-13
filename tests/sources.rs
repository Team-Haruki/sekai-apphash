//! Region sources: the QooApp store channel, channel headers and the ordered
//! fallback between sources. In-process HTTP servers only.

mod support;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use sekai_apphash::config::{
    ChannelConfig, Config, RegionConfig, Secret, SourceConfig, SourceKind, StateConfig,
};
use sekai_apphash::runner::{RunOptions, Runner};
use sekai_apphash::testing::{apk, xapk};
use sekai_apphash::ServerRegion;
use support::{PackageServer, QooApp, QooStore};

const TW_HASH: &str = "d67a688b-ff78-4300-bfc5-2120c000d75d";
const QOO_TOKEN: &str = "qoo-user-token-value";
const TW: &str = "com.hermes.mk.asia";
const TW_QOO: &str = "com.hermes.mk.asia.qooapp";

const RUN: RunOptions = RunOptions {
    force: false,
    publish: false,
};

struct Setup {
    store: QooStore,
    store_addr: std::net::SocketAddr,
    /// Serves the QooApp build (`com.hermes.mk.asia.qooapp`, a single APK).
    qoo_package: PackageServer,
    /// Serves the mirror build (an XAPK of `com.hermes.mk.asia`).
    mirror: PackageServer,
    mirror_addr: std::net::SocketAddr,
    state: tempfile::TempDir,
}

async fn setup() -> Setup {
    let qoo_package = PackageServer::new(apk(TW_QOO, ("6", "4", "1"), TW_HASH).bytes);
    let qoo_addr = qoo_package.start().await;
    let mirror = PackageServer::new(xapk(TW, ("6", "4", "1"), TW_HASH).bytes);
    let mirror_addr = mirror.start().await;
    let store = QooStore::new(QOO_TOKEN, format!("http://{qoo_addr}/pkg"));
    store.set(
        "75057",
        QooApp {
            package_id: TW_QOO.into(),
            version_name: "6.4.0".into(),
            md5: "0123abcd".into(),
            asset_pack_url: None,
        },
    );
    let store_addr = store.start().await;
    Setup {
        store,
        store_addr,
        qoo_package,
        mirror,
        mirror_addr,
        state: tempfile::tempdir().unwrap(),
    }
}

impl Setup {
    fn qoo_source(&self) -> SourceConfig {
        SourceConfig {
            name: "qooapp".into(),
            kind: SourceKind::Qooapp,
            check_url: format!("http://{}/store/v11/apps/75057", self.store_addr),
            download_url: format!(
                "http://{}/v11/apps/{{package_id}}/download",
                self.store_addr
            ),
            channel: "qoo".into(),
            packages: vec![TW.into(), TW_QOO.into()],
            ..SourceConfig::default()
        }
    }

    fn mirror_source(&self, path: &str) -> SourceConfig {
        SourceConfig {
            name: "mirror".into(),
            download_url: format!("http://{}{path}", self.mirror_addr),
            ..SourceConfig::default()
        }
    }

    fn config(&self, token: &str, sources: Vec<SourceConfig>) -> Config {
        let mut config = Config {
            state: StateConfig {
                dir: self.state.path().to_path_buf(),
            },
            ..Config::default()
        };
        config.regions.insert(
            ServerRegion::Tw,
            RegionConfig {
                sources,
                ..RegionConfig::default()
            },
        );
        let headers = [
            ("Accept", "*/*"),
            ("Accept-Encoding", "gzip, deflate, br"),
            ("Connection", "keep-alive"),
            ("User-Agent", "Mozilla/5.0 (test)"),
            ("x-version-name", "9.1.1"),
            ("x-device-id", ""),
            ("x_user_token", token),
        ];
        config.channels.insert(
            "qoo".into(),
            ChannelConfig {
                headers: headers
                    .into_iter()
                    .map(|(name, value)| (name.to_owned(), Secret::new(value)))
                    .collect::<BTreeMap<_, _>>(),
                required_headers: vec!["x-user-token".into()],
            },
        );
        config.validate().unwrap();
        config
    }
}

#[tokio::test]
async fn a_qooapp_source_resolves_through_the_store_api() {
    let s = setup().await;
    let config = s.config(QOO_TOKEN, vec![s.qoo_source()]);
    let runner = Runner::new(config.clone()).unwrap();

    let report = runner.run_once(&[ServerRegion::Tw], RUN).await.remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert!(report.extracted);
    let identity = report.identity.as_ref().unwrap();
    assert_eq!(identity.app_version, "6.4.1");
    assert_eq!(identity.app_hash, TW_HASH);
    let manifest = identity.manifest.as_ref().unwrap();
    assert_eq!(manifest.package, TW_QOO);
    assert_eq!(manifest.version_name.as_deref(), Some("6.4.0"));
    // Reported without the signed query of the redirect.
    let location = &identity.source.as_ref().unwrap().location;
    assert!(
        location.ends_with(&format!("/v11/apps/{TW_QOO}/download")),
        "{location}"
    );
    assert_eq!(s.store.store_requests.load(Ordering::SeqCst), 1);
    assert_eq!(s.store.downloads.load(Ordering::SeqCst), 1);

    // Channel headers went to the store and the download; transport headers
    // were left to the client.
    for headers in s.store.seen.lock().unwrap().iter() {
        assert_eq!(headers["x-user-token"], QOO_TOKEN);
        assert_eq!(headers["user-agent"], "Mozilla/5.0 (test)");
        assert_eq!(headers["x-version-name"], "9.1.1");
        assert_eq!(headers["x-device-id"], "");
        assert!(headers
            .get("accept-encoding")
            .is_none_or(|v| !v.to_str().unwrap().contains("gzip")));
    }

    // Same MD5: one store request, no download, cached identity.
    let ranges = s.qoo_package.requests.load(Ordering::SeqCst);
    let again = runner.run_once(&[ServerRegion::Tw], RUN).await.remove(0);
    assert!(again.is_ok() && !again.extracted, "{again:?}");
    assert_eq!(s.store.store_requests.load(Ordering::SeqCst), 2);
    assert_eq!(s.store.downloads.load(Ordering::SeqCst), 1);
    assert_eq!(s.qoo_package.requests.load(Ordering::SeqCst), ranges);

    // A new MD5 is a new build.
    let mut app = s.store.apps.lock().unwrap()["75057"].clone();
    app.md5 = "4567ef01".into();
    s.store.set("75057", app);
    let changed = runner.run_once(&[ServerRegion::Tw], RUN).await.remove(0);
    assert!(changed.is_ok() && changed.extracted, "{changed:?}");

    // The token never shows up in reports or debug output.
    let rendered = format!("{}{config:?}", serde_json::to_string(&changed).unwrap());
    assert!(!rendered.contains(QOO_TOKEN));
}

#[tokio::test]
async fn the_asset_pack_split_is_read_when_the_store_lists_it() {
    let s = setup().await;
    let split = format!(
        "{}?file=UnityDataAssetPack.apk&sig=signed-split",
        s.store.package_url.lock().unwrap()
    );
    let mut app = s.store.apps.lock().unwrap()["75057"].clone();
    app.asset_pack_url = Some(split);
    s.store.set("75057", app);
    let runner = Runner::new(s.config(QOO_TOKEN, vec![s.qoo_source()])).unwrap();
    let report = runner.run_once(&[ServerRegion::Tw], RUN).await.remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(s.store.downloads.load(Ordering::SeqCst), 0);
    let location = &report.identity.unwrap().source.unwrap().location;
    assert!(location.ends_with("/pkg"), "{location}");
}

#[tokio::test]
async fn store_package_names_must_be_allowed() {
    let s = setup().await;
    let mut source = s.qoo_source();
    source.packages.clear();
    let runner = Runner::new(s.config(QOO_TOKEN, vec![source])).unwrap();
    let report = runner.run_once(&[ServerRegion::Tw], RUN).await.remove(0);
    let error = report.error.unwrap();
    assert!(
        error.contains(&format!("package {TW_QOO} is not allowed for tw")),
        "{error}"
    );
}

#[tokio::test]
async fn sources_are_tried_in_order() {
    let s = setup().await;

    // The primary works: the fallback is never contacted.
    let runner =
        Runner::new(s.config(QOO_TOKEN, vec![s.mirror_source("/latest"), s.qoo_source()])).unwrap();
    let report = runner.run_once(&[ServerRegion::Tw], RUN).await.remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(report.identity.unwrap().manifest.unwrap().package, TW);
    assert_eq!(s.store.store_requests.load(Ordering::SeqCst), 0);

    // The primary fails (no byte ranges): the fallback answers.
    let runner =
        Runner::new(s.config(QOO_TOKEN, vec![s.mirror_source("/plain"), s.qoo_source()])).unwrap();
    let report = runner.run_once(&[ServerRegion::Tw], RUN).await.remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(report.identity.unwrap().manifest.unwrap().package, TW_QOO);
    assert_eq!(s.store.store_requests.load(Ordering::SeqCst), 1);

    // A disabled source is passed over.
    let mut disabled = s.qoo_source();
    disabled.enabled = false;
    let runner =
        Runner::new(s.config(QOO_TOKEN, vec![disabled, s.mirror_source("/latest")])).unwrap();
    assert!(runner.run_once(&[ServerRegion::Tw], RUN).await[0].is_ok());
    assert_eq!(s.store.store_requests.load(Ordering::SeqCst), 1);

    // Without the token the QooApp source is skipped without a request.
    let mirror_requests = s.mirror.requests.load(Ordering::SeqCst);
    let runner =
        Runner::new(s.config("", vec![s.qoo_source(), s.mirror_source("/latest")])).unwrap();
    let report = runner.run_once(&[ServerRegion::Tw], RUN).await.remove(0);
    assert!(report.is_ok(), "{report:?}");
    assert_eq!(s.store.store_requests.load(Ordering::SeqCst), 1);
    assert!(s.mirror.requests.load(Ordering::SeqCst) > mirror_requests);

    // Nothing works: every source is named in the error.
    let runner =
        Runner::new(s.config("", vec![s.mirror_source("/plain"), s.qoo_source()])).unwrap();
    let error = runner.run_once(&[ServerRegion::Tw], RUN).await[0]
        .error
        .clone()
        .unwrap();
    assert!(
        error.contains("mirror: http: range probe answered HTTP 200"),
        "{error}"
    );
    assert!(
        error.contains("qooapp: skipped (channel qoo lacks x-user-token)"),
        "{error}"
    );
    assert_eq!(s.store.store_requests.load(Ordering::SeqCst), 1);

    // A wrong token is an ordinary failure of that source.
    let runner = Runner::new(s.config("wrong", vec![s.qoo_source()])).unwrap();
    let error = runner.run_once(&[ServerRegion::Tw], RUN).await[0]
        .error
        .clone()
        .unwrap();
    assert!(error.contains("qooapp store answered HTTP 401"), "{error}");
}

#[tokio::test]
async fn switching_sources_re_extracts() {
    let s = setup().await;
    let runner = Runner::new(s.config(QOO_TOKEN, vec![s.mirror_source("/latest")])).unwrap();
    assert!(runner.run_once(&[ServerRegion::Tw], RUN).await[0].extracted);
    assert!(!runner.run_once(&[ServerRegion::Tw], RUN).await[0].extracted);
    // The QooApp build has its own fingerprint, so it is read once.
    let runner = Runner::new(s.config(QOO_TOKEN, vec![s.qoo_source()])).unwrap();
    let report = runner.run_once(&[ServerRegion::Tw], RUN).await.remove(0);
    assert!(report.is_ok() && report.extracted, "{report:?}");
}
