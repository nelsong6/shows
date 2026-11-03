#![forbid(unsafe_code)]

mod agent;
mod discovery;
mod picker;
mod ui;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;
use shows_control::{Command, resolve_show};
use shows_remote::transport;
use std::io::IsTerminal;
use transport::Session;

#[derive(Clone, Copy, ValueEnum)]
enum Channel {
    Stable,
    Dev,
}

#[derive(Parser)]
#[command(
    version = version(),
    about = "Control a running Shows desktop over your existing SSH connection.",
    after_help = "Examples:\n  shows-remote\n  shows-remote upstairspc play\n  shows-remote upstairspc play \"The Simpsons\"\n  shows-remote upstairspc status --json\n\nNo arguments opens the instance picker. The target must already have Shows open."
)]
struct Args {
    target: Option<String>,
    #[arg(long, value_enum, default_value = "stable", global = true)]
    channel: Channel,
    #[arg(
        long,
        global = true,
        help = "Print the result as JSON (requires a command)"
    )]
    json: bool,
    #[arg(long, hide = true)]
    agent: bool,
    #[arg(long, help = "Control Shows on this PC without SSH")]
    local: bool,
    #[command(subcommand)]
    command: Option<Action>,
}

fn version() -> String {
    format!(
        "{} ({})",
        env!("CARGO_PKG_VERSION"),
        option_env!("SHOWS_BUILD_SHA").unwrap_or("source")
    )
}

#[derive(Subcommand)]
enum Action {
    /// Show the target's current playback state.
    Status,
    /// List the target's current round and show IDs.
    Queue,
    /// Resume playback, or navigate to a show in the current round.
    Play { show: Option<String> },
    /// Pause playback (safe to repeat).
    Pause,
    /// Toggle fullscreen on the controlled TV, leaving playback unchanged.
    Fullscreen,
    /// Mark the current episode watched and move forward.
    Skip,
    /// Navigate back without marking watched.
    Previous,
    /// Move this episode to the back of its show's queue without marking watched.
    Defer,
    /// Seek by a relative number of seconds.
    Seek {
        #[arg(allow_hyphen_values = true)]
        seconds: f64,
    },
    /// Set volume from 0 to 130.
    Volume { level: f64 },
}

fn run() -> Result<(), String> {
    let args = Args::parse();
    if args.agent {
        return agent::run();
    }
    if args.command.is_none()
        && (args.json || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal())
    {
        return Err("The interactive remote needs a terminal. Use a command such as status --json for scripts.".into());
    }
    let channel = match args.channel {
        Channel::Stable => "stable",
        Channel::Dev => "dev",
    };
    if args.command.is_none() {
        let mut target = args.target.map(|host| discovery::Target {
            host,
            local: args.local,
        });
        if args.local && target.is_none() {
            target = Some(discovery::Target {
                host: "local".into(),
                local: true,
            });
        }
        loop {
            let explicit = target.is_some();
            let selected = match target.take() {
                Some(target) => Some(target),
                None => discovery::choose(channel)?,
            };
            let Some(selected) = selected else {
                return Ok(());
            };
            let session = if explicit {
                Session::connect(&selected.host, channel, selected.local)
            } else {
                Session::connect_native(&selected.host, channel, selected.local)
            }?;
            if !ui::run(session, &selected.host, channel, selected.local)? {
                return Ok(());
            }
        }
    }
    let target = args.target.as_deref().or(if args.local { Some("local") } else { None })
        .ok_or("Commands require a target: shows-remote <computer> status. Run shows-remote with no arguments to choose an instance.")?;
    let mut session = Session::connect(target, channel, args.local)?;
    let Some(action) = args.command else {
        unreachable!();
    };
    let command = match action {
        Action::Status => Command::Status {},
        Action::Queue => Command::Queue {},
        Action::Play { show: Some(query) } => Command::PlayShow {
            show_id: resolve_show(&session.call(Command::Status {})?, &query)?,
        },
        Action::Play { show: None } => Command::Play {},
        Action::Pause => Command::Pause {},
        Action::Fullscreen => Command::Fullscreen {},
        Action::Skip => Command::Skip {},
        Action::Previous => Command::Previous {},
        Action::Defer => Command::Defer {},
        Action::Seek { seconds } => Command::Seek { seconds },
        Action::Volume { level } => Command::Volume { level },
    };
    command.validate()?;
    let result = session.call(command.clone())?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
        );
    } else {
        println!("{}", if args.local { "Local Shows" } else { target });
        match command {
            Command::Status {} => print_status(&result),
            Command::Queue {} => {
                if let Some(round) = result.as_array() {
                    for (i, entry) in round.iter().enumerate() {
                        println!(
                            "{:>2}. {}  [{}]",
                            i + 1,
                            text(&entry["show_name"]),
                            text(&entry["show_id"])
                        );
                    }
                }
            }
            _ => println!("{}", text(&result["message"])),
        }
    }
    Ok(())
}

pub fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("—")
}

pub fn current(status: &Value) -> &Value {
    let pos = status["round_pos"].as_u64().unwrap_or(0) as usize;
    &status["round"][pos]
}

pub fn clock(value: &Value) -> String {
    let seconds = value
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0) as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn print_status(status: &Value) {
    let playback = &status["playback"];
    println!("{} — {}", text(&status["phase"]), text(&status["message"]));
    println!(
        "{} | {} / {} | {} | volume {}",
        text(&current(status)["show_name"]),
        clock(&playback["time_pos"]),
        clock(&playback["duration"]),
        if playback["paused"] == true {
            "paused"
        } else {
            "playing"
        },
        playback["volume"]
    );
    if let Some(alerts) = status["alerts"].as_array() {
        for alert in alerts {
            println!("{}", text(&alert["message"]));
        }
    }
}

fn main() {
    if let Err(message) = run() {
        eprintln!("shows-remote: {message}");
        std::process::exit(1);
    }
}
