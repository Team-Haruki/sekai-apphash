//! Per-region state: the last source fingerprint, the identity extracted for
//! it, and what each publisher last delivered. Lets a run skip downloads when
//! the package has not changed and skip deliveries that would repeat.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::identity::AppIdentity;
use crate::server::ServerRegion;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionState {
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub identity: Option<AppIdentity>,
    /// Publisher name -> digest of the last delivered payload.
    #[serde(default)]
    pub delivered: BTreeMap<String, String>,
}

/// Stores one JSON file per region in a directory; a store without a
/// directory keeps nothing.
#[derive(Debug, Clone)]
pub struct StateStore {
    dir: Option<PathBuf>,
}

impl StateStore {
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: (!dir.as_os_str().is_empty()).then(|| dir.to_path_buf()),
        }
    }

    #[must_use]
    pub const fn disabled() -> Self {
        Self { dir: None }
    }

    fn path(&self, region: ServerRegion) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|dir| dir.join(format!("{region}.json")))
    }

    /// Loads a region's state; a missing or unreadable file is an empty state.
    pub async fn load(&self, region: ServerRegion) -> RegionState {
        let Some(path) = self.path(region) else {
            return RegionState::default();
        };
        match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%region, %error, "Ignoring unreadable state file");
                RegionState::default()
            }),
            Err(_) => RegionState::default(),
        }
    }

    pub async fn save(&self, region: ServerRegion, state: &RegionState) -> Result<()> {
        let Some(path) = self.path(region) else {
            return Ok(());
        };
        write_atomic(&path, &serde_json::to_vec_pretty(state)?).await
    }
}

/// Writes through a temporary sibling and a rename, so readers never see a
/// partial file.
pub async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp-{}",
        path.extension().and_then(|e| e.to_str()).unwrap_or("json"),
        std::process::id()
    ));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// A short digest used to recognise a payload that was already delivered.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    hex::encode(&Sha256::digest(bytes)[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_and_tolerates_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::new(dir.path());
        assert_eq!(store.load(ServerRegion::Jp).await, RegionState::default());
        let mut state = RegionState {
            fingerprint: "size=1".into(),
            ..RegionState::default()
        };
        state.delivered.insert("file".into(), digest(b"x"));
        store.save(ServerRegion::Jp, &state).await.unwrap();
        assert_eq!(store.load(ServerRegion::Jp).await, state);
        tokio::fs::write(dir.path().join("jp.json"), b"{not json")
            .await
            .unwrap();
        assert_eq!(store.load(ServerRegion::Jp).await, RegionState::default());

        let disabled = StateStore::new(Path::new(""));
        disabled.save(ServerRegion::Jp, &state).await.unwrap();
        assert_eq!(
            disabled.load(ServerRegion::Jp).await,
            RegionState::default()
        );
        assert_eq!(StateStore::disabled().path(ServerRegion::En), None);
    }

    #[test]
    fn digest_is_stable() {
        assert_eq!(digest(b"abc"), digest(b"abc"));
        assert_ne!(digest(b"abc"), digest(b"abd"));
        assert_eq!(digest(b"").len(), 32);
        // Known answer: the first 16 bytes of SHA-256("abc"). Delivered
        // digests are persisted, so a crate bump must not change them.
        assert_eq!(digest(b"abc"), "ba7816bf8f01cfea414140de5dae2223");
    }
}
