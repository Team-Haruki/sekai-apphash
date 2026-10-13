# sekai-apphash

**sekai-apphash** extracts the app identity of `Project Sekai: Colorful Stage` (JP, EN, TW,
KR and CN) straight from the published game packages: the app version and app hash a client
sends to log in, plus everything else the client's `PlayerSettingConfig` carries (data and
asset-bundle versions, bundle version and code, product and company names, and the configs of
every build environment).

It reads packages with HTTP range requests, so a 500 MB XAPK costs about 2 MB (JP/EN) or 10 MB
(TW/KR/CN) of downloads instead of a full copy, and parses the Unity data with
[unity-rs](https://github.com/seiunx-dev/unity-rs). It is a library, a CLI, and an optional
long-running service that publishes changes to
[Haruki Sekai API](https://github.com/Team-Haruki/Haruki-Sekai-API), JSON files or webhooks.

## How it works

1. **Probe.** A `Range: bytes=0-0` request (following redirects) returns the package size,
   `ETag` and `Last-Modified`. When this fingerprint matches the last run, the cached identity
   is reused and nothing is downloaded.
2. **Zip.** The end-of-central-directory record and the central directory are read from the
   tail. Stored entries are addressed in place, which also opens the stored APKs inside an
   XAPK (`UnityDataAssetPack.apk`, the base APK) as nested archives.
3. **Manifest.** The base APK's binary `AndroidManifest.xml` gives the package name,
   `versionName` and `versionCode`. The package must belong to the requested region.
4. **Unity data.** The JP/EN builds keep the config in a small serialized file
   (`assets/bin/Data/6350e2ec327334c8a9b7f494f344a761`); the Nuverse builds pack it into
   `resources.assets` inside the LZ4 UnityFS bundle `assets/bin/Data/data.unity3d`. Only the
   bundle blocks covering the serialized-file metadata and the probed objects are fetched and
   decoded.
5. **Configs.** Player builds strip type trees, so the `Sekai.PlayerSettingConfig` layout is
   supplied as an external type tree and each candidate MonoBehaviour is read through
   unity-rs's type-tree reader, which rejects objects the layout does not describe exactly.
   The identity comes from `production_android` (or `production_ios`).

## Usage

```shell
cp config.example.toml config.toml   # fill in the package URLs per region

# Extract now and print JSON (no state, no publishing)
sekai-apphash -c config.toml extract
sekai-apphash -c config.toml extract jp kr --brief -o apphash.json
sekai-apphash extract tw --url https://example.com/latest.xapk
sekai-apphash extract cn --file ./game.apk

# One pass with caching and publishing; exits non-zero if any region failed.
# Suited to a systemd timer or cron.
sekai-apphash -c config.toml run
sekai-apphash -c config.toml run --force        # re-extract and re-deliver
sekai-apphash -c config.toml run --no-publish

# Long-running: a pass every schedule.interval_secs + random 0..=jitter_secs
sekai-apphash -c config.toml serve
```

Logs go to stderr (`log.format = "json"` for structured output); stdout carries the result:

```json
{
  "generatedAt": "2026-10-13T10:39:51Z",
  "identities": {
    "tw": {
      "region": "tw",
      "config": "production_android",
      "appVersion": "6.4.1",
      "appHash": "d67a688b-ff78-4300-bfc5-2120c000d75d",
      "dataVersion": "1.0.0",
      "abVersion": "6.4.0",
      "bundleVersion": "6.4.1",
      "bundleVersionCode": 2,
      "manifest": { "package": "com.hermes.mk.asia", "versionName": "6.4.0", "versionCode": 21899 },
      "unityVersion": "2022.3.62f3",
      "unityFile": "com.hermes.mk.asia.apk!/assets/bin/Data/data.unity3d#resources.assets",
      "configs": [ "... every PlayerSettingConfig in the build ..." ],
      "source": { "location": "...", "size": 344454400, "bytesFetched": 10482944 }
    }
  }
}
```

`dataVersion` is the client's bundled `clientData*Version`, not the server's master data
version. Nuverse servers accept `major.minor.0` of a build, so Haruki Sekai API stores TW/KR/CN
`6.4.1` as `6.4.0`; the extracted value is reported unchanged.

### Configuration

See [`config.example.toml`](config.example.toml). Every key can be overridden from the
environment with the `SEKAI_APPHASH_` prefix and `__` between levels, which is how secrets
should be supplied:

```shell
SEKAI_APPHASH_CONFIG=/etc/sekai-apphash/config.toml
SEKAI_APPHASH_REGIONS__CN__URL=https://...
SEKAI_APPHASH_PUBLISH__HARUKI__TOKEN=...
SEKAI_APPHASH_PUBLISH__WEBHOOKS__AUDIT__HMAC_SECRET=...
```

Each region may set its own `proxy` (unset inherits `http.proxy`, `""` connects directly), for
hosts that reach some download sources only through a proxy.

Tokens and secrets are never logged or printed (`Debug` shows `<redacted>`).

### Publishers

- **haruki** — `POST {base_url}/internal/app-identity` with `Authorization: Bearer <token>` and
  `{"server", "appVersion", "appHash"}`. The node's current identity is read first from
  `GET /internal/master/{region}/version`: nothing is sent when it already matches (after the
  node's Nuverse normalization), and an older version is refused unless
  `refuse_downgrade = false`. The publisher fails closed: when that read fails (network error,
  non-200 status such as the 404 of a node without `internal_token`, or an `{ok: false}`
  envelope such as a missing version file) nothing is sent and the region counts as failed, so
  `run` exits non-zero. With `refuse_downgrade` on, a node version that cannot be compared is
  refused the same way.
- **Bootstrapping a new region.** A node whose version file exists but has an empty
  `appVersion` has no identity to protect yet. The publisher still refuses it unless the region
  is listed in `bootstrap_regions` (e.g. `bootstrap_regions = ["kr"]`); the first identity is
  then pushed without the downgrade check. Once the node holds an identity, the normal checks
  apply again; remove the region from the list afterwards. A missing version file cannot be
  bootstrapped this way: the node refuses the `POST` as well, so create the file first.
- **file** — one JSON object keyed by region (`path`) and/or one `<region>.json` per region
  (`dir`), rewritten atomically when an identity changes.
- **webhooks** — `POST {"event":"app-identity","region":...,"identity":{...}}` when an identity
  changes, with optional `Authorization: Bearer` and an
  `X-Sekai-Apphash-Signature: sha256=<HMAC-SHA256 of the body>` header.

A failing publisher does not stop the others; `run` exits non-zero and `serve` reports it.

### Service endpoints

With `service.listen` set, `serve` answers `GET /health`
(`starting` / `ok` / `degraded`, per region), `GET /v1/identity` and
`GET /v1/identity/{region}`. They are read-only and unauthenticated; bind them to a private
address.

### Docker

```shell
docker run -d --name sekai-apphash \
  -v ./config.toml:/app/config.toml:ro -v sekai-apphash-state:/app/state \
  -e SEKAI_APPHASH_PUBLISH__HARUKI__TOKEN=... \
  ghcr.io/team-haruki/sekai-apphash:latest            # runs `serve`
docker run --rm -v ./config.toml:/app/config.toml:ro ghcr.io/team-haruki/sekai-apphash run
```

## Library

```rust
use sekai_apphash::{config::Config, runner::Runner, ServerRegion};

async fn jp_identity() -> sekai_apphash::Result<()> {
    let runner = Runner::new(Config::load(Some("config.toml".as_ref()))?)?;
    let identity = runner.extract(ServerRegion::Jp).await?;
    println!("{} {}", identity.app_version, identity.app_hash);
    Ok(())
}
```

Lower-level pieces are public too: `remote` (a page-cached HTTP range source implementing
unity-rs's `Source`), `zip`, `axml`, `bundle_source` (lazy UnityFS entries), `unity` and
`player_settings`.

## Development

```shell
cargo test                       # unit + offline integration tests (synthetic fixtures)
cargo clippy --all-targets -- -D warnings
SEKAI_APPHASH_CONFIG=config.toml cargo test --test live -- --ignored --nocapture
```

The offline tests build their packages from scratch (binary manifest, serialized file,
LZ4 UnityFS bundle, nested zip) and serve them over an in-process HTTP server with range
support; no game files are stored in this repository.

## Acknowledgements

The approach follows earlier community tools; no code was copied from them:

- [Sekai-World/sekai-apphash-updater](https://github.com/Sekai-World/sekai-apphash-updater)
  (MIT): the `PlayerSettingConfig` field layout and the Unity data file names.
- Resona AppHash Updater and Resona Sekai API (Go): reading APKs by range requests, decoding
  only the UnityFS blocks that hold the config, and reading the manifest version of the
  Nuverse builds.

## License

This project is licensed under the MIT License.
