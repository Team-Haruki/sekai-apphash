//! Structured logging to stderr (stdout carries the JSON output).

use tracing_subscriber::EnvFilter;

/// `RUST_LOG` wins; otherwise `level` applies to this crate and `warn` to
/// dependencies. `format` is `text` or `json`.
pub fn init(level: &str, format: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(format!("sekai_apphash={level},warn")))
        .unwrap_or_else(|_| EnvFilter::new("sekai_apphash=info,warn"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false);
    let _ = if format.eq_ignore_ascii_case("json") {
        builder.json().flatten_event(true).try_init()
    } else {
        builder
            .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
            .try_init()
    };
}
