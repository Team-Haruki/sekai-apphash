use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use sekai_apphash::config::{Config, ENV_CONFIG_PATH};
use sekai_apphash::runner::{Output, RegionReport, RunOptions, Runner};
use sekai_apphash::{logging, service, ServerRegion};
use tracing::error;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Extract the Project SEKAI app identity (app version, app hash, data
/// version) from published game packages.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// TOML config file. Without one, only environment overrides apply.
    #[arg(short, long, global = true, env = ENV_CONFIG_PATH)]
    config: Option<PathBuf>,

    /// Log level for this crate (RUST_LOG overrides it).
    #[arg(long, global = true)]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extract identities now and print them; no state, no publishing.
    Extract(ExtractArgs),
    /// One pass: probe each region, extract when its package changed, publish.
    /// Exits non-zero when any region failed (for a systemd timer).
    Run(RunArgs),
    /// Run passes on the configured schedule until stopped.
    Serve,
}

#[derive(Args)]
struct ExtractArgs {
    /// Regions to extract (default: every enabled region).
    regions: Vec<ServerRegion>,
    /// Extract from this URL instead of the configured one (one region).
    #[arg(long, conflicts_with = "file")]
    url: Option<String>,
    /// Extract from a local APK/XAPK/IPA (one region).
    #[arg(long)]
    file: Option<PathBuf>,
    /// `android` or `ios` production config.
    #[arg(long, default_value = "android")]
    platform: String,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Args)]
struct RunArgs {
    /// Regions to check (default: every enabled region).
    regions: Vec<ServerRegion>,
    /// Re-extract and re-deliver even if nothing changed.
    #[arg(long)]
    force: bool,
    /// Extract only; skip every publisher.
    #[arg(long)]
    no_publish: bool,
    #[command(flatten)]
    output: OutputArgs,
}

#[derive(Args)]
struct OutputArgs {
    /// Write the JSON result here instead of stdout (`-` for stdout).
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Single-line JSON.
    #[arg(long)]
    compact: bool,
    /// Omit the per-environment config list.
    #[arg(long)]
    brief: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = match Config::load(cli.config.as_deref()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("sekai-apphash: {e}");
            return ExitCode::from(2);
        }
    };
    logging::init(
        cli.log_level.as_deref().unwrap_or(&config.log.level),
        &config.log.format,
    );
    match run(cli.command, config).await {
        Ok(code) => code,
        Err(e) => {
            error!(error = %e, "Failed");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command, config: Config) -> anyhow::Result<ExitCode> {
    match command {
        Command::Extract(args) => extract(args, config).await,
        Command::Run(args) => {
            let regions = pick_regions(args.regions, &config)?;
            let runner = Runner::new(config)?;
            let reports = runner
                .run_once(
                    &regions,
                    RunOptions {
                        force: args.force,
                        publish: !args.no_publish,
                    },
                )
                .await;
            emit(&reports, &args.output).await?;
            Ok(exit_code(&reports))
        }
        Command::Serve => {
            service::serve(Runner::new(config)?).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn extract(args: ExtractArgs, config: Config) -> anyhow::Result<ExitCode> {
    let runner = Runner::new(config)?;
    let single = args.url.is_some() || args.file.is_some();
    let regions = if single {
        match args.regions.as_slice() {
            [region] => vec![*region],
            _ => anyhow::bail!("--url/--file take exactly one region"),
        }
    } else {
        pick_regions(args.regions, runner.config())?
    };
    let mut reports = Vec::new();
    for region in regions {
        let result = if let Some(path) = &args.file {
            Runner::extract_file(region, &args.platform, path).await
        } else if let Some(url) = &args.url {
            match runner.probe(region, url, &Default::default()).await {
                Ok(info) => {
                    runner
                        .extract_remote(region, &args.platform, &info, Default::default())
                        .await
                }
                Err(e) => Err(e),
            }
        } else {
            runner.extract(region).await
        };
        let (identity, error) = match result {
            Ok(identity) => (Some(identity), None),
            Err(e) => {
                error!(%region, error = %e, "Extraction failed");
                (None, Some(e.to_string()))
            }
        };
        reports.push(RegionReport {
            region,
            extracted: identity.is_some(),
            identity,
            error,
            publish_errors: Vec::new(),
            checked_at: chrono::Utc::now(),
        });
    }
    emit(&reports, &args.output).await?;
    Ok(exit_code(&reports))
}

fn pick_regions(
    requested: Vec<ServerRegion>,
    config: &Config,
) -> anyhow::Result<Vec<ServerRegion>> {
    if !requested.is_empty() {
        return Ok(requested);
    }
    let regions = config.enabled_regions();
    if regions.is_empty() {
        anyhow::bail!("no regions given and no enabled region has a url in the config");
    }
    Ok(regions)
}

async fn emit(reports: &[RegionReport], args: &OutputArgs) -> anyhow::Result<()> {
    let mut output = Output::from_reports(reports);
    if args.brief {
        for identity in output.identities.values_mut() {
            identity.configs.clear();
        }
    }
    let mut bytes = if args.compact {
        serde_json::to_vec(&output)?
    } else {
        serde_json::to_vec_pretty(&output)?
    };
    bytes.push(b'\n');
    match &args.output {
        Some(path) if path.as_os_str() != "-" => {
            // Regular files are replaced atomically; devices and pipes are
            // written in place.
            let special = std::fs::metadata(path).is_ok_and(|m| !m.is_file());
            if special {
                tokio::fs::write(path, &bytes).await?;
            } else {
                sekai_apphash::state::write_atomic(path, &bytes).await?;
            }
        }
        _ => {
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
        }
    }
    Ok(())
}

fn exit_code(reports: &[RegionReport]) -> ExitCode {
    if reports.iter().all(RegionReport::is_ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
