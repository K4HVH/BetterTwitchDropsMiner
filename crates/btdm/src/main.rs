mod auth_flow;
mod config;
mod notify;
mod run;
mod storage;
mod supervisor;
mod update;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "btdm", about = "A Twitch drops miner that tells you the truth")]
struct Args {
    /// Where state.db and config.toml live. Defaults to $XDG_DATA_HOME/btdm.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Which configured account to act on.
    #[arg(long, global = true, default_value = "default")]
    account: String,

    #[arg(long, global = true, default_value = "info")]
    log_level: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Link a Twitch account using the device-code flow.
    Login {
        /// Discard any existing token and start over.
        #[arg(long)]
        force: bool,
    },
    /// Show which accounts are linked and whether their tokens still work.
    Status,
    /// Remove a stored token.
    Logout,
    /// Mine drops until interrupted, with the dashboard.
    Mine {
        /// Do not start the web dashboard.
        #[arg(long)]
        headless: bool,
        /// Mine only the --account given, rather than every linked account.
        #[arg(long)]
        only_this_account: bool,
    },
    /// Run a single pass and report what the miner sees, then exit.
    Probe,
    /// Check for a newer release. Installs nothing unless --apply is given.
    Update {
        /// Download and install the update, replacing this binary.
        #[arg(long)]
        apply: bool,
    },
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("btdm")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| args.log_level.clone().into()),
        )
        .init();

    let data_dir = args.data_dir.unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir)?;

    let config_path = data_dir.join("config.toml");
    let cfg = config::Config::load(&config_path)?;
    if !config_path.exists() {
        cfg.save_atomic(&config_path)?;
        tracing::info!(path = %config_path.display(), "wrote a default config");
    }

    let storage = storage::Storage::open(&data_dir.join("state.db"))?;

    match args.command {
        Command::Login { force } => auth_flow::login(&storage, &args.account, force).await,
        Command::Status => auth_flow::status(&storage, &args.account).await,
        Command::Logout => {
            if storage.clear_tokens(&args.account)? {
                println!("Removed the stored token for account '{}'.", args.account);
            } else {
                println!("No stored token found for account '{}'.", args.account);
            }
            Ok(())
        }
        Command::Mine {
            headless,
            only_this_account,
        } => {
            supervisor::run(
                std::sync::Arc::new(storage),
                cfg,
                supervisor::SupervisorOptions {
                    headless,
                    only: only_this_account.then(|| args.account.clone()),
                },
            )
            .await
        }
        Command::Update { apply } => update::run(apply).await,
        Command::Probe => {
            run::mine(
                &storage,
                &args.account,
                &cfg,
                run::RunOptions { probe: true },
                None,
            )
            .await
        }
    }
}
