use btdm::{auth_flow, config, control_cli, run, storage, supervisor, update};

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "btdm",
    version,
    about = "A Twitch drops miner that tells you the truth"
)]
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
    /// Show what the running miner is doing.
    Status {
        /// Emit the raw snapshot instead, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Show which accounts are linked and whether their tokens still work.
    Accounts,
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
    /// Change what a running miner is doing. Same controls as the dashboard.
    #[command(subcommand)]
    Control(ControlCommand),
}

/// Every one of these reaches a *running* miner over its HTTP API and takes
/// effect immediately — they are a client of `btdm mine`, not a second way of
/// mining. Nothing here requires a restart or a hand-edited config file, and
/// every one of them has a control on the dashboard that does the same thing
/// through the same endpoint.
#[derive(Subcommand)]
enum ControlCommand {
    /// Show what every running account is doing. Same as `btdm status`.
    #[command(hide = true)]
    Status {
        /// Emit the raw snapshot instead, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Stop mining. Already-earned drops are still claimed.
    Pause,
    /// Start mining again.
    Resume,
    /// Change the selection strategy.
    Strategy {
        /// One of: deadline, priority, coverage, completionist, manual.
        name: String,
    },
    /// Watch only this channel, under the manual strategy.
    Pin {
        /// Channel login. Omit with --clear to unpin.
        channel: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Switch to a specific channel now, without changing the strategy.
    Watch { channel: String },
    /// Leave the current channel and choose again.
    Switch,
    /// Re-fetch campaigns now.
    Sync,
    /// Prefer a channel when the choice is otherwise close.
    Favourite {
        channel: String,
        /// Remove it instead.
        #[arg(long)]
        remove: bool,
    },
    /// Set the priority order the priority strategies rank by.
    ///
    /// Entries match a game name *or* a campaign name, so two campaigns for
    /// the same game can be ranked against each other. Order matters: the
    /// first entry wins.
    Priority {
        /// The full list, best first. Replaces whatever is there.
        #[arg(
            required_unless_present_any = ["add", "remove", "clear", "up", "down"],
            conflicts_with_all = ["add", "remove", "clear", "up", "down"],
        )]
        games: Vec<String>,
        /// Append one entry instead of replacing the list.
        #[arg(long, conflicts_with_all = ["remove", "clear", "up", "down"])]
        add: Option<String>,
        /// Drop one entry, leaving the rest in order.
        #[arg(long, conflicts_with_all = ["clear", "up", "down"])]
        remove: Option<String>,
        /// Move one entry one place towards the front.
        #[arg(long, conflicts_with_all = ["clear", "down"])]
        up: Option<String>,
        /// Move one entry one place towards the back.
        #[arg(long, conflicts_with = "clear")]
        down: Option<String>,
        /// Empty the list.
        #[arg(long)]
        clear: bool,
    },
    /// Never mine a game's campaigns.
    ExcludeGame {
        game: String,
        /// Undo the exclusion.
        #[arg(long)]
        undo: bool,
    },
    /// Never mine a specific campaign, by id.
    ExcludeCampaign {
        id: String,
        #[arg(long)]
        undo: bool,
    },
    /// Forget every channel and campaign penalty and try them all again.
    ClearPenalties,
}

fn to_command(c: ControlCommand) -> Option<btdm_core::control::Command> {
    use btdm_core::control::Command as C;
    Some(match c {
        // Handled separately: they read the current state before writing.
        ControlCommand::Status { .. } | ControlCommand::Priority { .. } => return None,
        ControlCommand::Pause => C::Pause,
        ControlCommand::Resume => C::Resume,
        ControlCommand::Strategy { name } => C::SetStrategy { name },
        ControlCommand::Pin { channel, clear } => C::Pin {
            channel: if clear { None } else { channel },
        },
        ControlCommand::Watch { channel } => C::WatchChannel { login: channel },
        ControlCommand::Switch => C::SwitchChannel,
        ControlCommand::Sync => C::SyncNow,
        ControlCommand::Favourite { channel, remove } => C::SetFavourite {
            login: channel,
            on: !remove,
        },
        ControlCommand::ExcludeGame { game, undo } => C::SetGameExcluded {
            game,
            excluded: !undo,
        },
        ControlCommand::ExcludeCampaign { id, undo } => C::SetCampaignExcluded {
            campaign: btdm_core::model::CampaignId(id),
            excluded: !undo,
        },
        ControlCommand::ClearPenalties => C::ClearQuarantine,
    })
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
        Command::Accounts => auth_flow::status(&storage, &args.account).await,
        Command::Status { json } => control_cli::show(&cfg, json).await,
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
                    config_store: config::ConfigStore::new(config_path),
                },
            )
            .await
        }
        Command::Update { apply } => update::run(apply).await,
        Command::Control(sub) => match sub {
            ControlCommand::Status { json } => control_cli::show(&cfg, json).await,
            ControlCommand::Priority { games, add, remove, up, down, clear } => {
                use control_cli::PriorityChange as P;
                // clap enforces that at most one of these is present.
                let change = if clear {
                    P::Clear
                } else if let Some(g) = add {
                    P::Add(g)
                } else if let Some(g) = remove {
                    P::Remove(g)
                } else if let Some(g) = up {
                    P::Move { entry: g, up: true }
                } else if let Some(g) = down {
                    P::Move { entry: g, up: false }
                } else {
                    P::Set(games)
                };
                control_cli::set_priority(&cfg, &args.account, change).await
            }
            other => match to_command(other) {
                // `to_command` returns None only for the two handled above.
                None => unreachable!("every other control command maps to a command"),
                Some(command) => control_cli::send(&cfg, &args.account, command).await,
            },
        },
        Command::Probe => {
            run::mine(
                &storage,
                &args.account,
                &cfg,
                run::RunOptions {
                    probe: true,
                    config_store: None,
                },
                None,
            )
            .await
        }
    }
}
