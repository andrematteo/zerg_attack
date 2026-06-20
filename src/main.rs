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
