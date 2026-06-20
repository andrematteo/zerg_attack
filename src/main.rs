//! `zerg_attack` — the CLI front-end for the `zerg` load tester.
//!
//! Subcommands: `local` (run on this machine), `hatchery` (coordinator),
//! `drone` (worker), `start` (trigger a fleet run), and `overlord` (live
//! telemetry dashboard).

use std::{path::PathBuf, time::Duration};

use clap::{Parser, Subcommand};
use zerg::{
    BenchmarkResult,
    protocol::{StartRequest, StartResponse},
    runner,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a load test locally (single machine).
    Local(RunArgs),
    /// Start the Hatchery (coordinator) and wait for Drones + a start command.
    Hatchery(HatcheryArgs),
    /// Start a Drone (worker) that registers with a Hatchery and runs work.
    Drone(DroneArgs),
    /// Send a start command to a running Hatchery.
    Start(StartArgs),
    /// Live telemetry dashboard: polls a Hatchery's /status and renders it.
    Overlord(OverlordArgs),
}

#[derive(Parser, Debug)]
struct RunArgs {
    #[arg(short, long, default_value_t = String::from("http://127.0.0.1:8090"))]
    url: String,
    #[arg(short, long, value_parser = humantime::parse_duration, default_value = "10s")]
    duration: Duration,
    #[arg(short, long, default_value_t = 400)]
    concurrency: usize,
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    #[arg(long, value_parser = humantime::parse_duration, default_value = "15s")]
    timeout: Duration,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    har: Option<PathBuf>,
    #[arg(long)]
    token: Option<String>,
}

#[derive(Parser, Debug)]
struct HatcheryArgs {
    /// Address to listen on.
    #[arg(short, long, default_value = "0.0.0.0:7700")]
    bind: String,
}

#[derive(Parser, Debug)]
struct DroneArgs {
    /// Hatchery base URL, e.g. http://10.0.0.1:7700
    #[arg(short = 'H', long)]
    hatchery: String,
    /// Preferred drone name (the Hatchery assigns drone-NN if omitted).
    #[arg(short, long)]
    name: Option<String>,
    /// Cap advertised concurrency regardless of socket limit.
    #[arg(long)]
    max_concurrency: Option<usize>,
    /// Exit after completing a single work order.
    #[arg(long)]
    once: bool,
}

#[derive(Parser, Debug)]
struct StartArgs {
    /// Hatchery base URL, e.g. http://10.0.0.1:7700
    #[arg(short = 'H', long)]
    hatchery: String,
    #[arg(short, long, default_value_t = String::from("http://127.0.0.1:8090"))]
    url: String,
    #[arg(short, long, value_parser = humantime::parse_duration, default_value = "10s")]
    duration: Duration,
    #[arg(short, long, default_value_t = 400)]
    concurrency: usize,
    #[arg(short, long, default_value_t = 4)]
    threads: usize,
    #[arg(long, value_parser = humantime::parse_duration, default_value = "15s")]
    timeout: Duration,
    #[arg(long)]
    har: Option<PathBuf>,
    #[arg(long)]
    token: Option<String>,
}

#[derive(Parser, Debug)]
struct OverlordArgs {
    /// Hatchery base URL, e.g. http://10.0.0.1:7700
    #[arg(short = 'H', long)]
    hatchery: String,
    /// Refresh interval.
    #[arg(short, long, value_parser = humantime::parse_duration, default_value = "1s")]
    interval: Duration,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Local(args) => run_local(args),
        Command::Hatchery(args) => run_hatchery(args),
        Command::Drone(args) => run_drone(args),
        Command::Start(args) => run_start(args),
        Command::Overlord(args) => zerg::overlord::run(&args.hatchery, args.interval),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

/// Normalizes a token into an `Authorization` value, adding the `Bearer ` prefix
/// unless the caller already supplied one.
fn bearer_from(token: Option<String>) -> Option<String> {
    token.map(|t| {
        let t = t.trim();
        if t.to_ascii_lowercase().starts_with("bearer ") {
            t.to_string()
        } else {
            format!("Bearer {t}")
        }
    })
}

fn run_local(args: RunArgs) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bearer = bearer_from(args.token);
    let har_content = match &args.har {
        Some(path) => Some(std::fs::read_to_string(path)?),
        None => None,
    };

    let results = runner::run_scenario(
        &args.url,
        har_content.as_deref(),
        bearer.as_deref(),
        args.concurrency,
        args.threads,
        args.duration,
        args.timeout,
    )?;

    if args.json {
        zerg::table::write_report_json("results.json", &results)?;
        eprintln!("Wrote results.json");
    } else {
        zerg::table::print_endpoint_report(&results);
    }

    Ok(())
}

fn run_hatchery(args: HatcheryArgs) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let hatchery = zerg::hatchery::Hatchery::new();
        let app = hatchery.clone().router();

        let listener = tokio::net::TcpListener::bind(&args.bind).await?;
        eprintln!("Hatchery: listening on {}", args.bind);
        eprintln!("Hatchery: waiting for Drones to register, then a `start` command.");

        let reporter = hatchery.clone();
        tokio::spawn(async move {
            let mut last_run: Option<String> = None;
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Some(report) = reporter.consolidated().await
                    && last_run.as_deref() != Some(report.run_id.as_str())
                {
                    last_run = Some(report.run_id.clone());
                    let results: Vec<(String, BenchmarkResult)> =
                        report.per_endpoint.into_iter().collect();
                    eprintln!("\n=== Consolidated report ({}) ===", report.run_id);
                    zerg::table::print_endpoint_report(&results);
                }
            }
        });

        axum::serve(listener, app).await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    })
}

fn run_drone(args: DroneArgs) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    zerg::drone::run(&args.hatchery, args.name, args.max_concurrency, args.once)
}

fn run_start(args: StartArgs) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let har_content = match &args.har {
        Some(path) => Some(std::fs::read_to_string(path)?),
        None => None,
    };
    let token = bearer_from(args.token);

    let req = StartRequest {
        url: args.url,
        duration: args.duration,
        concurrency: args.concurrency,
        threads: args.threads,
        timeout: args.timeout,
        token,
        har_content,
    };

    let client = reqwest::blocking::Client::new();
    let resp: StartResponse = client
        .post(format!("{}/start", args.hatchery))
        .json(&req)
        .send()?
        .json()?;

    match resp {
        StartResponse::Accepted { assignments } => {
            eprintln!(
                "Start accepted. Load split across {} drone(s):",
                assignments.len()
            );
            for a in assignments {
                eprintln!(
                    "  {}: concurrency={}, threads={}",
                    a.drone_id, a.concurrency, a.threads
                );
            }
            eprintln!("Watch the Hatchery terminal for the consolidated report.");
        }
        StartResponse::Rejected { message } => {
            eprintln!("Start rejected: {message}");
            std::process::exit(2);
        }
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory; // for Cli::command()
    use clap::Parser; // for Cli::try_parse_from()

    // ---- bearer_from -------------------------------------------------------

    #[test]
    fn bearer_from_none_stays_none() {
        assert_eq!(bearer_from(None), None);
    }

    #[test]
    fn bearer_from_adds_prefix_to_bare_token() {
        assert_eq!(
            bearer_from(Some("abc123".to_string())),
            Some("Bearer abc123".to_string())
        );
    }

    #[test]
    fn bearer_from_does_not_double_prefix() {
        assert_eq!(
            bearer_from(Some("Bearer abc123".to_string())),
            Some("Bearer abc123".to_string())
        );
    }

    #[test]
    fn bearer_from_prefix_match_is_case_insensitive() {
        // Detection is case-insensitive, but the original casing is preserved
        // (the function returns the trimmed input unchanged when it already
        // looks like a Bearer value).
        assert_eq!(
            bearer_from(Some("bearer abc".to_string())),
            Some("bearer abc".to_string())
        );
        assert_eq!(
            bearer_from(Some("BEARER abc".to_string())),
            Some("BEARER abc".to_string())
        );
    }

    #[test]
    fn bearer_from_trims_surrounding_whitespace() {
        assert_eq!(
            bearer_from(Some("   abc   ".to_string())),
            Some("Bearer abc".to_string())
        );
        // Already-prefixed and padded: trimmed, then recognized as Bearer.
        assert_eq!(
            bearer_from(Some("  Bearer xyz  ".to_string())),
            Some("Bearer xyz".to_string())
        );
    }

    // ---- edge cases that document current behavior (worth reviewing) -------

    #[test]
    fn bearer_from_word_bearer_without_space_is_treated_as_a_token() {
        // "bearer" with no trailing space does NOT match the "bearer " prefix,
        // so it is treated as a raw token and prefixed. Documents the quirk.
        assert_eq!(
            bearer_from(Some("bearer".to_string())),
            Some("Bearer bearer".to_string())
        );
    }

    #[test]
    fn bearer_from_empty_string_yields_bearer_space() {
        // An empty/whitespace-only token currently produces "Bearer " (a
        // dangling prefix). Flagged — you may prefer to return None here.
        assert_eq!(
            bearer_from(Some("   ".to_string())),
            Some("Bearer ".to_string())
        );
    }

    // ---- clap: the command definition itself is valid ----------------------

    #[test]
    fn cli_definition_is_valid() {
        // Catches structural mistakes at test time: duplicate short flags,
        // bad arg config, etc.
        Cli::command().debug_assert();
    }

    #[test]
    fn version_flag_is_wired() {
        let err = Cli::try_parse_from(["zerg_attack", "--version"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    // ---- clap: `local` defaults & overrides --------------------------------

    #[test]
    fn local_uses_documented_defaults() {
        let cli = Cli::try_parse_from(["zerg_attack", "local"]).unwrap();
        match cli.command {
            Command::Local(a) => {
                assert_eq!(a.url, "http://127.0.0.1:8090");
                assert_eq!(a.duration, Duration::from_secs(10));
                assert_eq!(a.concurrency, 400);
                assert_eq!(a.threads, 4);
                assert_eq!(a.timeout, Duration::from_secs(15));
                assert!(!a.json);
                assert!(a.har.is_none());
                assert!(a.token.is_none());
            }
            other => panic!("expected Local, got {other:?}"),
        }
    }

    #[test]
    fn local_overrides_are_parsed() {
        let cli = Cli::try_parse_from([
            "zerg_attack",
            "local",
            "--url",
            "https://api.example.com",
            "-c",
            "800",
            "-t",
            "8",
            "--duration",
            "1m30s",
            "--timeout",
            "5s",
            "--json",
            "--token",
            "abc",
        ])
        .unwrap();
        match cli.command {
            Command::Local(a) => {
                assert_eq!(a.url, "https://api.example.com");
                assert_eq!(a.concurrency, 800);
                assert_eq!(a.threads, 8);
                assert_eq!(a.duration, Duration::from_secs(90)); // humantime 1m30s
                assert_eq!(a.timeout, Duration::from_secs(5));
                assert!(a.json);
                assert_eq!(a.token.as_deref(), Some("abc"));
            }
            other => panic!("expected Local, got {other:?}"),
        }
    }

    #[test]
    fn invalid_duration_is_rejected() {
        let err = Cli::try_parse_from(["zerg_attack", "local", "--duration", "banana"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    // ---- clap: `hatchery` --------------------------------------------------

    #[test]
    fn hatchery_bind_default() {
        let cli = Cli::try_parse_from(["zerg_attack", "hatchery"]).unwrap();
        match cli.command {
            Command::Hatchery(a) => assert_eq!(a.bind, "0.0.0.0:7700"),
            other => panic!("expected Hatchery, got {other:?}"),
        }
    }

    // ---- clap: `drone` -----------------------------------------------------

    #[test]
    fn drone_requires_hatchery() {
        let err = Cli::try_parse_from(["zerg_attack", "drone"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn drone_parses_flags() {
        let cli = Cli::try_parse_from([
            "zerg_attack",
            "drone",
            "-H",
            "http://10.0.0.1:7700",
            "--name",
            "drone-a",
            "--max-concurrency",
            "256",
            "--once",
        ])
        .unwrap();
        match cli.command {
            Command::Drone(a) => {
                assert_eq!(a.hatchery, "http://10.0.0.1:7700");
                assert_eq!(a.name.as_deref(), Some("drone-a"));
                assert_eq!(a.max_concurrency, Some(256));
                assert!(a.once);
            }
            other => panic!("expected Drone, got {other:?}"),
        }
    }

    #[test]
    fn drone_optional_defaults() {
        let cli = Cli::try_parse_from(["zerg_attack", "drone", "-H", "http://h:7700"]).unwrap();
        match cli.command {
            Command::Drone(a) => {
                assert!(a.name.is_none());
                assert!(a.max_concurrency.is_none());
                assert!(!a.once);
            }
            other => panic!("expected Drone, got {other:?}"),
        }
    }

    // ---- clap: `start` -----------------------------------------------------

    #[test]
    fn start_requires_hatchery() {
        let err = Cli::try_parse_from(["zerg_attack", "start"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn start_inherits_load_defaults() {
        let cli =
            Cli::try_parse_from(["zerg_attack", "start", "-H", "http://h:7700"]).unwrap();
        match cli.command {
            Command::Start(a) => {
                assert_eq!(a.hatchery, "http://h:7700");
                assert_eq!(a.url, "http://127.0.0.1:8090");
                assert_eq!(a.duration, Duration::from_secs(10));
                assert_eq!(a.concurrency, 400);
                assert_eq!(a.threads, 4);
                assert_eq!(a.timeout, Duration::from_secs(15));
                assert!(a.har.is_none());
                assert!(a.token.is_none());
            }
            other => panic!("expected Start, got {other:?}"),
        }
    }

    // ---- clap: `overlord` --------------------------------------------------

    #[test]
    fn overlord_requires_hatchery() {
        let err = Cli::try_parse_from(["zerg_attack", "overlord"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn overlord_interval_default_and_override() {
        let def = Cli::try_parse_from(["zerg_attack", "overlord", "-H", "http://h:7700"]).unwrap();
        match def.command {
            Command::Overlord(a) => assert_eq!(a.interval, Duration::from_secs(1)),
            other => panic!("expected Overlord, got {other:?}"),
        }

        let ovr = Cli::try_parse_from([
            "zerg_attack",
            "overlord",
            "-H",
            "http://h:7700",
            "--interval",
            "250ms",
        ])
        .unwrap();
        match ovr.command {
            Command::Overlord(a) => assert_eq!(a.interval, Duration::from_millis(250)),
            other => panic!("expected Overlord, got {other:?}"),
        }
    }
}
