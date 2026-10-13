//! Live extraction against real package sources. Ignored by default: it needs
//! network access and source URLs, which this repository does not ship.
//!
//! ```sh
//! SEKAI_APPHASH_CONFIG=config.toml cargo test --test live -- --ignored --nocapture
//! ```
//!
//! Every enabled region of the config (or of `SEKAI_APPHASH_REGIONS__<R>__URL`
//! overrides) is extracted; nothing is published and no state is written.

use std::path::PathBuf;

use sekai_apphash::config::Config;
use sekai_apphash::identity::version_parts;
use sekai_apphash::runner::Runner;

#[tokio::test]
#[ignore = "needs network access and configured package sources"]
async fn extracts_every_configured_region() {
    let path = std::env::var_os("SEKAI_APPHASH_CONFIG").map(PathBuf::from);
    let mut config = Config::load(path.as_deref()).expect("config loads");
    config.publish = Default::default();
    config.state.dir = PathBuf::new();
    let regions = config.enabled_regions();
    assert!(!regions.is_empty(), "configure at least one region url");
    let runner = Runner::new(config).unwrap();
    for region in regions {
        let identity = runner
            .extract(region)
            .await
            .unwrap_or_else(|e| panic!("{region}: {e}"));
        println!(
            "{region}: appVersion={} appHash={} dataVersion={} manifest={:?} fetched={:?}",
            identity.app_version,
            identity.app_hash,
            identity.data_version,
            identity
                .manifest
                .as_ref()
                .and_then(|m| m.version_name.clone()),
            identity.source.as_ref().and_then(|s| s.bytes_fetched),
        );
        assert_eq!(
            version_parts(&identity.app_version).map(|p| p.len()),
            Some(3)
        );
        assert_eq!(
            identity.app_hash.len(),
            36,
            "{region}: unexpected hash shape"
        );
        let package = identity.manifest.as_ref().map(|m| m.package.as_str());
        assert_eq!(
            package,
            Some(region.package_name()),
            "{region}: wrong package"
        );
    }
}
