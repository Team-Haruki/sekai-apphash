use std::io;

/// Errors raised while locating, downloading and parsing a game package.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(String),
    /// No HTTP answer: the connection, TLS handshake or request failed or timed
    /// out. Usually a network or proxy blip, so worth trying again.
    #[error("http: {0}")]
    Transient(String),
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

impl Error {
    /// True for failures that a later attempt may not repeat (see
    /// [`Error::Transient`]); never for an HTTP status or an unusable answer.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        let transient = is_transient(&error);
        // reqwest includes the URL in its messages; source URLs are not secret,
        // but publisher URLs may carry credentials in the query, so strip it.
        let message = chain(&error.without_url());
        if transient {
            Self::Transient(message)
        } else {
            Self::Http(message)
        }
    }
}

/// Connection, TLS and timeout failures, and a request that got no status
/// (e.g. the peer closed the connection during the handshake).
fn is_transient(error: &reqwest::Error) -> bool {
    if error.is_connect() || error.is_timeout() || (error.is_request() && error.status().is_none())
    {
        return true;
    }
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        if let Some(io) = inner.downcast_ref::<io::Error>() {
            return matches!(
                io.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::UnexpectedEof
            );
        }
        source = inner.source();
    }
    false
}

/// The error and its sources, `outer: inner: ...` (reqwest alone only says
/// "error sending request"). A source already quoted by its parent is skipped.
#[must_use]
pub fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(inner) = source {
        let text = inner.to_string();
        if !text.is_empty() && !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = inner.source();
    }
    message
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Other(format!("json: {error}"))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    /// A local address nothing listens on.
    fn closed_port() -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }

    #[tokio::test]
    async fn a_refused_connection_is_transient_and_explained() {
        let addr = closed_port();
        let error = reqwest::Client::new()
            .get(format!("http://{addr}/check?token=secret"))
            .send()
            .await
            .unwrap_err();
        let error = Error::from(error);
        assert!(error.is_transient(), "{error:?}");
        let message = error.to_string();
        // More than reqwest's bare "error sending request", and no URL.
        assert!(
            message.starts_with("http: error sending request: "),
            "{message}"
        );
        assert!(
            !message.contains("secret") && !message.contains("/check"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_peer_closing_the_connection_is_transient() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });
        let error = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap_err();
        assert!(Error::from(error).is_transient());
    }

    #[test]
    fn statuses_and_bad_answers_are_not_transient() {
        assert!(!Error::Http("qooapp store answered HTTP 404 Not Found".into()).is_transient());
        assert!(!Error::Http("qooapp store answer has no data".into()).is_transient());
        let json = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(!Error::from(json).is_transient());
        assert!(!Error::Io(io::Error::other("range 0-1: HTTP 404")).is_transient());
    }

    #[test]
    fn the_chain_names_every_distinct_cause() {
        #[derive(Debug)]
        struct Wrap(&'static str, Option<Box<Wrap>>);
        impl std::fmt::Display for Wrap {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Wrap {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_deref().map(|e| e as _)
            }
        }
        let error = Wrap(
            "error sending request",
            Some(Box::new(Wrap(
                "client error (Connect)",
                Some(Box::new(Wrap(
                    "client error (Connect)",
                    Some(Box::new(Wrap("Connection refused (os error 111)", None))),
                ))),
            ))),
        );
        assert_eq!(
            chain(&error),
            "error sending request: client error (Connect): Connection refused (os error 111)"
        );
    }
}
