# AGENTS.md — AI Agent Guidelines for sekai-apphash

## Project Summary

sekai-apphash extracts the app identity (app version, app hash and the rest of the client's
`PlayerSettingConfig`) of Project Sekai's JP, EN, TW, KR and CN builds from published game
packages, reading them with HTTP range requests and parsing the Unity data with `unity-rs`.
It is a library, a CLI (`extract`, `run`, `serve`) and a long-running scheduler that publishes
changes to Haruki-Sekai-API (`POST /internal/app-identity`), JSON files and webhooks.

This repository is **public**: never commit infrastructure details (hostnames, IPs, ports of
real deployments, server paths), real download endpoints, or tokens. Examples use
`example.com` / `example.invalid` and `127.0.0.1`.

## Repository Structure

```
src/
  main.rs            – CLI (clap): extract / run / serve; JSON to stdout or --output
  lib.rs             – Module list and crate docs
  server.rs          – ServerRegion (jp/en/tw/kr/cn), package names, CP vs Nuverse
  remote.rs          – HTTP range probe + PagedSource (page cache, coalesced ranges)
                       implementing unity-rs `Source`; RemoteInfo fingerprint
  zip.rs             – Read-only zip over a Region (EOCD, zip64, stored subregions,
                       deflate, nested stored APKs)
  axml.rs            – Binary AndroidManifest: package, versionName, versionCode
  bundle_source.rs   – Lazy random-access Source over one UnityFS entry (LZ4/none blocks)
  unity.rs           – Find and decode PlayerSettingConfig objects in a bundle or a
                       serialized file (type grouping, size pre-filter)
  player_settings.rs – External TypeTree for Sekai.PlayerSettingConfig (Android/Base
                       layouts) and the decoded struct
  extract.rs         – Package walk: manifest + Unity data candidates + nested APKs
  identity.rs        – AppIdentity output model, version comparison, Nuverse normalization
  config.rs          – TOML + SEKAI_APPHASH_* env overrides (figment), Secret type
  state.rs           – Per-region state (fingerprint, identity, delivery digests)
  publish.rs         – Haruki / file / webhook publishers
  runner.rs          – One pass: probe -> reuse cache or extract -> publish; source
                       HTTP clients (global and per-region proxy)
  service.rs         – Scheduler with jitter, read-only status endpoints (axum)
  logging.rs         – tracing to stderr, text or JSON
  testing.rs         – Synthetic fixture builders (doc-hidden; used by all tests)
tests/
  pipeline.rs        – Offline end-to-end tests over in-process HTTP servers
  live.rs            – #[ignore] live extraction (needs network + configured sources)
  support/           – Range server, mock Haruki node, webhook receiver
```

## Key Concepts

- **Range reading.** Everything is a `unity_rs_core::source::Region`. A remote package is a
  `PagedSource<HttpFetcher>`; reads block on the tokio runtime handle, so extraction always
  runs inside `spawn_blocking`. Never call it from an async task directly.
- **Where the config lives.** JP/EN: a bare serialized file
  `assets/bin/Data/6350e2ec327334c8a9b7f494f344a761` in the asset-pack or base APK. TW/KR/CN:
  `resources.assets` inside `assets/bin/Data/data.unity3d` (UnityFS, LZ4 blocks). The Nuverse
  builds also carry debug/staging/promotion configs; the identity is `production_<platform>`.
  Their `bundleIdentifier` is the JP one, so only the manifest package is checked against the
  region.
- **No type trees.** Player builds strip them, so `player_settings::type_tree` supplies the
  layout and unity-rs's `read_type_tree_from_reader_with_reference_types` reads the object; it
  fails unless the tree consumes the object exactly, which is what tells layouts apart. When
  the game adds or reorders fields, update `STRING_FIELDS` / the Android trailer and the
  synthetic encoder in `testing.rs` together.
- **Versions.** `appVersion` is `clientMajor.clientMinor.clientBuild` as extracted (Nuverse
  6.4.1). Haruki-Sekai-API normalizes Nuverse versions to `major.minor.0`;
  `identity::server_app_version` mirrors that only for the publisher's comparison and
  downgrade check, never in the output.
- **Haruki contract.** `POST /internal/app-identity` with `Authorization: Bearer
  <backend.internal_token>` and `{server, appVersion, appHash}`; the node answers HTTP 200 with
  an envelope `{ok, data: {changed, appVersion, appHash}}` or `{ok: false, kind, message}`, 401
  for a wrong token and 404 when its internal endpoints are disabled.
  `GET /internal/master/{region}/version` returns the version file (`appVersion`, `appHash`, ...).
- **Caching.** `state/<region>.json` keeps the source fingerprint (size, ETag, Last-Modified,
  redirect path without query) and the identity. Unchanged fingerprint = no download.
  File/webhook deliveries are skipped when the stable digest (identity without timestamps and
  download stats) was already delivered; the Haruki publisher compares with the node instead.

## Coding Standards

- Rust 2021, async with tokio; `reqwest` (rustls) for HTTP; `axum` for the status server.
- The binary sets `mimalloc` as the global allocator.
- Library errors are `error::Error` (thiserror); `anyhow` only in `main.rs` and tests.
- Logging: `tracing` with structured fields (`region = %region`), to stderr; never log tokens,
  secrets or full publisher URLs. Credentials in config are `config::Secret`.
- `unity-rs-core` is pinned with `=`; bumping it is a deliberate change that needs the live
  test (below) to pass for all five regions.
- No real game files in the repository (copyright); extend `testing.rs` instead.
- Comments only where the logic is not obvious.

## Testing

- `cargo test` runs unit tests (in-module) and `tests/pipeline.rs`; both are offline.
- `tests/live.rs` is `#[ignore]`d: `SEKAI_APPHASH_CONFIG=config.toml cargo test --test live --
  --ignored --nocapture` extracts every enabled region without publishing.
- Tests that touch the environment use `figment::Jail` (it serializes them).

## Building

```bash
cargo build
cargo build --release          # thin LTO, codegen-units = 1, panic = abort, stripped
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
docker build -t sekai-apphash .
```

The Dockerfile is a cargo-chef multi-stage musl build (`rust:1.99-alpine` -> `alpine:3.24`);
the image runs `sekai-apphash serve` as user `haruki`, reads `/app/config.toml`
(`SEKAI_APPHASH_CONFIG`) and keeps state in `/app/state`.

## Git commits

All commit subjects must follow:

```text
[Type] Short description starting with capital letter
```

| Type      | Usage                                                 |
|-----------|-------------------------------------------------------|
| `[Feat]`  | New feature or capability                             |
| `[Fix]`   | Bug fix                                               |
| `[Chore]` | Maintenance, refactoring, dependency or build changes |
| `[Docs]`  | Documentation-only changes                            |

- Imperative mood, capitalized, no trailing period, about 70 characters at most.
- Agent attribution uses the standard `Co-authored-by:` trailer in the commit body, separated
  from the subject by a blank line (e.g. `Co-authored-by: Claude Opus 4.7 <noreply@anthropic.com>`,
  substituting the actual model).

## GitHub Actions workflows

CI and releases reuse the shared workflows in
[seiunx-dev/ci-templates](https://github.com/seiunx-dev/ci-templates) (`@v1`); the files in
`.github/workflows` are thin callers. Reuse the templates first; add a custom job only when a
template genuinely cannot do it, with a comment saying why, and fix template gaps upstream.

- `ci.yml` (`CI`): main pushes, PRs to main, manual dispatch. Rust lint (fmt + Clippy
  `-D warnings`), tests once under cargo-llvm-cov, MSRV check (`rust-version`, 1.94), Sonar
  (skips without `SONAR_TOKEN`), Docker, actionlint.
- `CI OK` is the only required status check.
- Docker: PRs build only when Docker inputs change. On main the image pushes
  `ghcr.io/team-haruki/sekai-apphash:sha-<sha>` at once; `Docker tags` moves `:main` after
  `CI OK`.
- `release.yml` (`Release`): `v*` tags and manual dispatch (dry run). Flow: bump `version` in
  `Cargo.toml` in a PR -> merge -> wait for `CI OK` on main -> push tag `v<version>` -> the gate
  checks the tag against Cargo and waits for `CI OK` -> archives (`sekai-apphash-<label>`) are
  built on the tag, the main `:sha-<sha>` image is re-tagged `:X.Y.Z`, `:X.Y`, `:latest` -> the
  GitHub Release is published and its notes rewritten to the org standard
  ([RELEASE_NOTES.md](https://github.com/seiunx-dev/ci-templates/blob/main/RELEASE_NOTES.md),
  English, no infrastructure details).
- Keep `permissions` minimal; pin third-party actions in custom steps to a full SHA.
