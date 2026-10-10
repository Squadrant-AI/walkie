// Removed in Task 11, once every module is wired up.
#![allow(dead_code)]

mod exit;
mod frame;
mod invite;
mod model;
mod paths;
mod proto;
mod session;
mod ticket;

use clap::{Parser, Subcommand};
use exit::{exit_of, fail, Exit};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "walkie", version, about = "Walkie-talkie for AI agents")]
struct Cli {
    /// Room id (else $WALKIE_ROOM, else the only active room)
    #[arg(long, global = true, env = "WALKIE_ROOM")]
    room: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open a room and print an invite
    Open {
        #[arg(long, default_value = "10m")]
        ttl: humantime::Duration,
        #[arg(long, default_value = "24h")]
        idle_ttl: humantime::Duration,
        #[arg(long)]
        relay_only: bool,
    },
    /// Join a room with an invite
    Join { invite: String },
    /// Send a message ("-" reads stdin)
    Send {
        text: String,
        #[arg(long)]
        reply_to: Option<String>,
    },
    /// Wait for messages/events
    Wait {
        #[arg(long, default_value_t = 90)]
        timeout: u64,
        #[arg(long)]
        json: bool,
    },
    /// Show room and peer status
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List rooms on this machine
    Rooms,
    /// Leave the room (it stays alive on the peer's side)
    Leave,
    /// Rejoin a room you left
    Rejoin { room_id: Option<String> },
    /// End the room for both sides
    End,
    #[command(name = "__daemon", hide = true)]
    Daemon {
        room_id: String,
        #[arg(long)]
        mode: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code.code()),
        Err(e) => {
            eprintln!("walkie: {e:#}");
            ExitCode::from(exit_of(&e).code())
        }
    }
}

fn run(_cli: Cli) -> anyhow::Result<Exit> {
    Err(fail(Exit::Error, "not implemented yet"))
}
