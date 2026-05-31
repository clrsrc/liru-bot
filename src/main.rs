//! Starting point for liru-bot (Rust port).

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Url;
use tracing::{error, info};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use liru_bot::config::Config;
use liru_bot::lichess::Lichess;
use liru_bot::VERSION;

#[derive(Debug, Parser)]
#[command(name = "liru-bot", version, about = "Run a chess bot on lichess.org")]
struct Args {
    /// Path to the YAML config file.
    #[arg(short, long, global = true, default_value = "./config.yml")]
    config: PathBuf,

    /// Log filter, e.g. `info`, `debug`, `liru_bot=debug,reqwest=info`.
    /// Overrides `RUST_LOG` when set.
    #[arg(short, long, global = true)]
    log: Option<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the bot (default — used when no subcommand is given).
    Run,

    /// List currently online bots, optionally filtered by minimum rating.
    ListBots {
        /// Show only bots with at least this rating in the considered
        /// speed(s). `0` (default) disables the filter.
        #[arg(long, default_value_t = 0)]
        min_rating: i64,

        /// Restrict the filter to one speed: `bullet`, `blitz`,
        /// `rapid`, `classical`, or `ultraBullet`. Omit to keep any bot
        /// that meets `min_rating` in at least one of those speeds.
        #[arg(long)]
        speed: Option<String>,

        /// How many bots Lichess should return (server default is 100,
        /// max ~300). The full list is filtered locally afterwards.
        #[arg(long, default_value_t = 300)]
        limit: u32,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    init_logging(args.log.as_deref());
    info!(version = VERSION, "liru-bot (Rust port) starting");
    match dispatch(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("fatal: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(override_filter: Option<&str>) {
    let filter = override_filter
        .map(|s| EnvFilter::new(s))
        .or_else(|| std::env::var("RUST_LOG").ok().map(EnvFilter::new))
        .unwrap_or_else(|| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).with_level(true))
        .init();
}

async fn dispatch(args: &Args) -> Result<()> {
    match &args.cmd {
        None | Some(Cmd::Run) => run_bot(args).await,
        Some(Cmd::ListBots { min_rating, speed, limit }) => {
            list_bots(args, *min_rating, speed.as_deref(), *limit).await
        }
    }
}

async fn run_bot(args: &Args) -> Result<()> {
    let config = Config::load(&args.config)
        .with_context(|| format!("loading config {}", args.config.display()))?;
    let (mut li, _) = connect(&config).await?;
    let profile = li.get_profile().await.context("fetching bot profile")?;
    info!(
        username = ?profile.username,
        id = ?profile.id,
        "connected to Lichess"
    );
    liru_bot::start_program(li, profile, config).await
}

async fn list_bots(
    args: &Args,
    min_rating: i64,
    speed: Option<&str>,
    limit: u32,
) -> Result<()> {
    let config = Config::load(&args.config)
        .with_context(|| format!("loading config {}", args.config.display()))?;
    let (li, _) = connect(&config).await?;
    let bots = li.get_online_bots(Some(limit)).await;
    if bots.is_empty() {
        eprintln!("(no bots returned by Lichess)");
        return Ok(());
    }

    let single_speed = speed.map(|s| s.to_string());

    let mut rows: Vec<BotRow> = bots
        .iter()
        .filter_map(|b| {
            let username = b.username.clone()?;
            let row = BotRow {
                username,
                bullet: b.rating_for("bullet"),
                blitz: b.rating_for("blitz"),
                rapid: b.rating_for("rapid"),
                classical: b.rating_for("classical"),
                ultra_bullet: b.rating_for("ultraBullet"),
            };
            let passes = match &single_speed {
                Some(only) => row.rating_for(only).map_or(false, |r| r >= min_rating),
                None => DEFAULT_SPEEDS
                    .iter()
                    .any(|s| row.rating_for(s).map_or(false, |r| r >= min_rating)),
            };
            if passes { Some(row) } else { None }
        })
        .collect();

    rows.sort_by_key(|r| -best_rating(r, single_speed.as_deref()));

    print_bot_table(&rows, single_speed.as_deref());
    eprintln!("({} bots matched)", rows.len());
    Ok(())
}

const DEFAULT_SPEEDS: &[&str] = &["bullet", "blitz", "rapid", "classical"];

fn best_rating(row: &BotRow, single: Option<&str>) -> i64 {
    match single {
        Some(s) => row.rating_for(s).unwrap_or(0),
        None => DEFAULT_SPEEDS
            .iter()
            .filter_map(|s| row.rating_for(s))
            .max()
            .unwrap_or(0),
    }
}

#[derive(Debug)]
struct BotRow {
    username: String,
    bullet: Option<i64>,
    blitz: Option<i64>,
    rapid: Option<i64>,
    classical: Option<i64>,
    ultra_bullet: Option<i64>,
}

impl BotRow {
    fn rating_for(&self, speed: &str) -> Option<i64> {
        match speed.to_lowercase().as_str() {
            "bullet" => self.bullet,
            "blitz" => self.blitz,
            "rapid" => self.rapid,
            "classical" => self.classical,
            "ultrabullet" => self.ultra_bullet,
            _ => None,
        }
    }
}

fn print_bot_table(rows: &[BotRow], single_speed: Option<&str>) {
    if let Some(s) = single_speed {
        println!("{:<24} {:>6}", "username", s);
        for r in rows {
            let v = r
                .rating_for(s)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into());
            println!("{:<24} {:>6}", r.username, v);
        }
    } else {
        println!(
            "{:<24} {:>6} {:>6} {:>6} {:>6}",
            "username", "bullet", "blitz", "rapid", "classic"
        );
        for r in rows {
            println!(
                "{:<24} {:>6} {:>6} {:>6} {:>6}",
                r.username,
                fmt_opt(r.bullet),
                fmt_opt(r.blitz),
                fmt_opt(r.rapid),
                fmt_opt(r.classical),
            );
        }
    }
}

fn fmt_opt(v: Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "-".into())
}

async fn connect(config: &Config) -> Result<(Lichess, ())> {
    let url = Url::parse(&config.url).with_context(|| format!("parsing url {:?}", config.url))?;
    let li = Lichess::connect(config.token.clone(), url, VERSION.to_string(), 3)
        .await
        .context("connecting to lichess.org")?;
    Ok((li, ()))
}
