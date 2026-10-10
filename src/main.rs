mod cli;
mod daemon;
mod exit;
mod frame;
mod invite;
mod ipc;
mod model;
mod net;
mod paths;
mod proto;
mod session;
mod ticket;

use clap::{Parser, Subcommand};
use exit::{exit_of, Exit};
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

fn run(cli: Cli) -> anyhow::Result<Exit> {
    if let Cmd::Daemon { room_id, mode } = cli.cmd {
        let mode = daemon::Mode::parse(&mode)?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let r = rt.block_on(daemon::run(room_id, mode));
        // don't let a lingering blocking task (DNS, etc.) keep the process alive
        rt.shutdown_background();
        r?;
        return Ok(Exit::Ok);
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let room = cli.room;
    rt.block_on(async move {
        match cli.cmd {
            Cmd::Open {
                ttl,
                idle_ttl,
                relay_only,
            } => cli::open(*ttl.as_ref(), *idle_ttl.as_ref(), relay_only).await,
            Cmd::Join { invite } => cli::join(&invite).await,
            Cmd::Send { text, reply_to } => cli::send(room, &text, reply_to).await,
            Cmd::Wait { timeout, json } => cli::wait(room, timeout, json).await,
            Cmd::Status { json } => cli::status(room, json).await,
            Cmd::Rooms => cli::rooms().await,
            Cmd::Leave => cli::leave(room).await,
            Cmd::Rejoin { room_id } => cli::rejoin(room, room_id).await,
            Cmd::End => cli::end(room).await,
            Cmd::Daemon { .. } => unreachable!("handled above"),
        }
    })
}
