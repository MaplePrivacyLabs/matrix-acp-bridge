use std::{collections::BTreeSet, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use matrix_acp_bridge::{
    config::Config,
    core::Bridge,
    model::*,
    offline::{Behavior, FixtureAgent},
    runner::Runner,
    store::Store,
};

#[derive(Parser)]
#[command(version, about = "Matrix + ACP worker and offline development tools")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a private config interactively; no connections, passwords or agents.
    Init {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
        /// Use the separate Linux service users created by ops/install-linux.sh.
        #[arg(long)]
        service_layout: bool,
    },
    /// Check ACP initialization, authentication and modes without sending a prompt.
    Doctor {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Validate policy syntax without reading credentials or connecting anywhere.
    Check {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
        /// Print effective access policy and binding hashes, never environment secrets.
        #[arg(long)]
        json: bool,
    },
    /// Exercise the real ACP SDK against an in-memory deterministic fixture.
    Demo {
        #[arg(value_enum, default_value = "conversation")]
        scenario: Scenario,
    },
    /// Enroll a separate Matrix bot device; prompts locally for its password.
    #[cfg(feature = "matrix")]
    Enroll {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Inspect identity and message trust without printing message bodies or secrets.
    #[cfg(feature = "matrix")]
    Status {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
    },
    /// Inspect one encrypted event in the single configured room.
    #[cfg(feature = "matrix")]
    InspectEvent {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
        event_id: String,
    },
    /// Check fetched thread context without printing message bodies or running an agent.
    #[cfg(feature = "matrix")]
    InspectContext {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
        event_id: String,
    },
    /// Compare a configured room member's SAS emojis with stock Element.
    #[cfg(feature = "matrix")]
    Verify {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
        user_id: String,
    },
    /// Expose the running bridge's local Matrix tools over MCP stdio.
    #[cfg(feature = "matrix")]
    Tools {
        #[arg(long)]
        socket: PathBuf,
    },
    /// Run the encrypted Matrix and ACP bridge in the foreground.
    #[cfg(feature = "matrix")]
    Run {
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,
        /// Explicitly reconsider one missed event through normal admission.
        /// Already admitted events remain deduplicated; sync cursors are unchanged.
        #[arg(long)]
        retry_event: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Scenario {
    Conversation,
    Approval,
    Cancellation,
    Recovery,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::Init {
            config,
            service_layout,
        } => {
            matrix_acp_bridge::setup::interactive(&config, service_layout)?;
        }
        Commands::Doctor { config } => {
            let config = Config::parse(&std::fs::read_to_string(config)?)?;
            let report = matrix_acp_bridge::acp::inspect(
                matrix_acp_bridge::acp::ScopedProcess(config.harness.clone()),
                config.harness,
            )
            .await?;
            println!(
                "ACP v1 connected. Available modes: {}",
                report.modes.join(", ")
            );
            println!("Session continuation supported: {}", report.can_resume);
            anyhow::ensure!(
                report.mode_applied,
                "configured mode is unavailable; choose one of the modes above"
            );
            anyhow::ensure!(
                report.can_resume,
                "agent cannot load/resume sessions; thread follow-ups require it"
            );
            println!("Configured mode applied. No prompt was sent; no Matrix connection was made.");
        }
        Commands::Check { config, json } => {
            let config = Config::parse(&std::fs::read_to_string(config)?)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "config_fingerprint":config.fingerprint(),
                        "max_concurrent_runs":config.max_concurrent_runs,
                        "approval_ttl_seconds":config.approval_ttl_seconds,
                        "harness_mode":config.harness.mode,
                        "rooms":config.rooms.iter().map(|r|serde_json::json!({
                            "room_id":r.room_id,"binding":config.binding_fingerprint(&r.room_id),
                            "operators":r.operators,"operator_trust":r.operator_trust,
                            "audience_policy":r.audience_policy,"tool_approval":r.tool_approval,
                        })).collect::<Vec<_>>()
                    })
                );
                return Ok(());
            }
            println!(
                "Configuration valid: {} room(s), {} concurrent run(s). No connections or agents started.",
                config.rooms.len(),
                config.max_concurrent_runs
            );
        }
        Commands::Demo { scenario } => demo(scenario).await?,
        #[cfg(feature = "matrix")]
        Commands::Enroll { config } => {
            let config = Config::parse(&std::fs::read_to_string(config)?)?;
            matrix_acp_bridge::live::enroll(config).await?;
        }
        #[cfg(feature = "matrix")]
        Commands::Status { config } => {
            let config = Config::parse(&std::fs::read_to_string(config)?)?;
            matrix_acp_bridge::live::status(config).await?;
        }
        #[cfg(feature = "matrix")]
        Commands::InspectEvent { config, event_id } => {
            let config = Config::parse(&std::fs::read_to_string(config)?)?;
            matrix_acp_bridge::live::inspect_event(config, &event_id).await?;
        }
        #[cfg(feature = "matrix")]
        Commands::InspectContext { config, event_id } => {
            let config = Config::parse(&std::fs::read_to_string(config)?)?;
            matrix_acp_bridge::live::inspect_context(config, &event_id).await?;
        }
        #[cfg(feature = "matrix")]
        Commands::Verify { config, user_id } => {
            let config = Config::parse(&std::fs::read_to_string(config)?)?;
            matrix_acp_bridge::live::verify(config, &user_id).await?;
        }
        #[cfg(feature = "matrix")]
        Commands::Tools { socket } => {
            matrix_acp_bridge::matrix_tools::proxy(&socket).await?;
        }
        #[cfg(feature = "matrix")]
        Commands::Run {
            config,
            retry_event,
        } => {
            let config = Config::parse(&std::fs::read_to_string(config)?)?;
            matrix_acp_bridge::live::run(config, retry_event.as_deref()).await?;
        }
    }
    Ok(())
}

async fn demo(scenario: Scenario) -> Result<()> {
    println!(
        "OFFLINE FIXTURE — in-memory ACP, no Matrix connection, AI model, tools or child process."
    );
    let config = Config::parse(include_str!("../config/example.toml"))?;
    let room = RoomSnapshot {
        joined: true,
        encrypted: true,
        members: config.rooms[0].audience.clone(),
    };
    let message = |id: &str, body: &str, thread: Option<&str>| Incoming {
        event_id: format!("${id}"),
        room_id: config.rooms[0].room_id.clone(),
        sender: "@owner:example.invalid".into(),
        body: body.into(),
        attachment: None,
        thread_root: thread.map(str::to_owned),
        reply_to: None,
        mentions: BTreeSet::from([config.bot_user_id.clone()]),
        encrypted: true,
        verified_device: true,
        known_sender_device: true,
    };
    let store = Store::memory(&config.bot_user_id)?;
    let bridge = Bridge::new(config.clone(), store)?;
    let behavior = match scenario {
        Scenario::Approval => Behavior::Approval,
        Scenario::Cancellation => Behavior::WaitForCancel,
        _ => Behavior::Reply,
    };
    let fixture = FixtureAgent::new(behavior);
    let for_factory = fixture.clone();
    let mut runner = Runner::new(bridge, Arc::new(move |_| for_factory.transport()))
        .with_clock(Arc::new(|| 101));
    let first = message("demo", "Summarize our fixture workspace", None);
    if matches!(scenario, Scenario::Recovery) {
        runner.bridge.handle(&first, &room, 100)?;
        runner.bridge.store.recover(101)?;
    } else {
        runner.ingest(&first, &room, 100).await?;
        drive_demo(&mut runner, scenario, &room, &message).await?;
        if matches!(scenario, Scenario::Conversation) {
            runner
                .ingest(
                    &message("followup", "Now explain the tests", Some("$demo")),
                    &room,
                    103,
                )
                .await?;
            drive_demo(&mut runner, scenario, &room, &message).await?;
        }
    }
    for output in runner.bridge.store.pending()? {
        println!(
            "\n[fixture Matrix thread {}]\n{}",
            output.conversation.thread_root.as_deref().unwrap_or("room"),
            output.body
        );
    }
    println!("\nFixture complete. Nothing was posted to Matrix.");
    runner.shutdown(110).await?;
    Ok(())
}

async fn drive_demo(
    runner: &mut Runner,
    scenario: Scenario,
    room: &RoomSnapshot,
    message: &impl Fn(&str, &str, Option<&str>) -> Incoming,
) -> Result<()> {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), runner.next_update()).await??;
        match event {
            AgentEvent::Permission { request_id, .. } if matches!(scenario, Scenario::Approval) => {
                runner
                    .ingest(
                        &message(
                            "approval",
                            &format!("!bridge approve {request_id} permit-once"),
                            Some("$demo"),
                        ),
                        room,
                        102,
                    )
                    .await?;
            }
            AgentEvent::SessionReady { .. } if matches!(scenario, Scenario::Cancellation) => {
                runner
                    .ingest(&message("cancel", "!bridge stop", Some("$demo")), room, 102)
                    .await?;
            }
            AgentEvent::Finished { .. } => break,
            _ => {}
        }
    }
    Ok(())
}
