use std::io;

/// Errors raised while locating, downloading and parsing a game package.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("zip: {0}")]
    Zip(String),
    #[error("unity: {0}")]
    Unity(#[from] unity_rs_core::Error),
    #[error("android manifest: {0}")]
    Manifest(String),
    #[error("app identity not found: {0}")]
    NotFound(String),
    #[error("config: {0}")]
    Config(String),
    #[error("publish: {0}")]
    Publish(String),
    #[error("{0}")]
    Other(String),
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        // reqwest includes the URL in its messages; source URLs are not secret,
        // but publisher URLs may carry credentials in the query, so strip it.
        Self::Http(error.without_url().to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Other(format!("json: {error}"))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
