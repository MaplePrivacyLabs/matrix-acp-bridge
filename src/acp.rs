//! Official ACP v1 client. No model, credentials, Matrix login, or subprocess is
//! needed by the protocol tests: a deterministic SDK agent connects in memory.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, ConnectTo, ConnectionTo, UntypedMessage,
    schema::{ProtocolVersion, v1::*},
};
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::{mpsc, oneshot};

use crate::{
    config::{HarnessConfig, Steering},
    model::{AgentEvent, PermissionOption as Choice, Run, RunStatus},
};

#[derive(Debug)]
pub enum Command {
    Cancel,
    Steer {
        event_id: String,
        prompt: String,
    },
    Decide {
        request_id: String,
        option_id: Option<String>,
    },
}

struct Pending {
    options: BTreeSet<String>,
    answer: oneshot::Sender<Option<String>>,
}

type PendingMap = Arc<Mutex<BTreeMap<String, Pending>>>;

fn error(message: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(message.to_string())
}

fn cancel_pending(pending: &PendingMap) {
    for (_, entry) in std::mem::take(&mut *pending.lock().expect("pending mutex poisoned")) {
        let _ = entry.answer.send(None);
    }
}

/// Preflight results. A new empty session is created, but no prompt is sent.
#[derive(Debug)]
pub struct Inspection {
    pub modes: Vec<String>,
    pub can_resume: bool,
    pub mode_applied: bool,
}

pub async fn inspect(
    transport: impl ConnectTo<Client>,
    harness: HarnessConfig,
) -> anyhow::Result<Inspection> {
    Client
        .builder()
        .on_receive_request(
            async move |_request: RequestPermissionRequest, responder, _cx| {
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            let init = tokio::time::timeout(
                Duration::from_secs(30),
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task(),
            )
            .await
            .map_err(|_| error("ACP initialize timed out; check executable and runtime PATH"))??;
            if init.protocol_version != ProtocolVersion::V1 {
                return Err(error("agent selected an unsupported protocol"));
            }
            let session = tokio::time::timeout(
                Duration::from_secs(30),
                connection
                    .send_request(NewSessionRequest::new(harness.workspace))
                    .block_task(),
            )
            .await
            .map_err(|_| error("ACP session creation timed out; check adapter authentication"))??;
            let modes: Vec<_> = session
                .modes
                .map(|m| {
                    m.available_modes
                        .into_iter()
                        .map(|m| m.id.to_string())
                        .collect()
                })
                .unwrap_or_default();
            let mode_applied = harness
                .mode
                .as_ref()
                .is_none_or(|mode| modes.contains(mode));
            if let Some(mode) = harness.mode.filter(|_| mode_applied) {
                tokio::time::timeout(
                    Duration::from_secs(30),
                    connection
                        .send_request(SetSessionModeRequest::new(session.session_id, mode))
                        .block_task(),
                )
                .await
                .map_err(|_| error("ACP mode change timed out"))??;
            }
            Ok(Inspection {
                modes,
                mode_applied,
                can_resume: init.agent_capabilities.load_session
                    || init
                        .agent_capabilities
                        .session_capabilities
                        .resume
                        .is_some(),
            })
        })
        .await
        .map_err(Into::into)
}

/// Run a conversation, resuming the stored session and accepting steering. Model turns have
/// no hidden time/token limit. Handshakes and requested cancellation have deadlines.
pub async fn execute(
    transport: impl ConnectTo<Client>,
    harness: HarnessConfig,
    run: Run,
    mcp_servers: Vec<McpServer>,
    approval_ttl: Duration,
    mut commands: mpsc::Receiver<Command>,
    updates: mpsc::Sender<AgentEvent>,
) -> anyhow::Result<()> {
    let pending: PendingMap = Arc::default();
    let active = Arc::new(AtomicBool::new(false));
    let expected_session: Arc<Mutex<Option<String>>> = Arc::default();
    let (mode_fault, mut mode_fault_rx) = mpsc::channel::<()>(1);
    let notification_updates = updates.clone();
    let notification_run = run.id.clone();
    let notification_active = active.clone();
    let notification_session = expected_session.clone();
    let required_mode = harness.mode.clone();
    let permission_pending = pending.clone();
    let permission_updates = updates.clone();
    let permission_run = run.id.clone();
    let permission_active = active.clone();
    let permission_session = expected_session.clone();
    let completion_updates = updates.clone();
    let completion_run = run.id.clone();
    let cleanup_pending = pending.clone();
    let result = Client.builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                let matches_session = notification_session.lock().expect("session mutex poisoned").as_deref() == Some(notification.session_id.to_string().as_str());
                if !notification_active.load(Ordering::SeqCst) || !matches_session { return Ok(()); }
                let event = match notification.update {
                    SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
                        ContentBlock::Text(text) => Some(AgentEvent::Text {run_id:notification_run.clone(),text:text.text}),
                        _ => None,
                    },
                    SessionUpdate::ToolCall(call) => Some(AgentEvent::Tool {run_id:notification_run.clone(),title:call.title}),
                    SessionUpdate::CurrentModeUpdate(mode) if required_mode.as_ref().is_some_and(|required| mode.current_mode_id.to_string() != *required) => {
                        let _ = mode_fault.try_send(());
                        None
                    }
                    // Never publish internal reasoning or replayed/user-echo content.
                    _ => None,
                };
                if let Some(event) = event { notification_updates.send(event).await.map_err(|_| error("controller closed"))?; }
                Ok(())
            }, agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, connection| {
                let matches_session = permission_session.lock().expect("session mutex poisoned").as_deref() == Some(request.session_id.to_string().as_str());
                if !permission_active.load(Ordering::SeqCst) || !matches_session || request.options.is_empty() {
                    return responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled));
                }
                let choices: Vec<_> = request.options.iter().map(|o| Choice {
                    id:o.option_id.to_string(),label:o.name.clone(),
                    kind:serde_json::to_value(o.kind).ok().and_then(|v|v.as_str().map(str::to_owned)).unwrap_or_else(||"unknown".into()),
                }).collect();
                let ids: BTreeSet<_> = choices.iter().map(|o|o.id.clone()).collect();
                if ids.len() != choices.len() || ids.iter().any(|id| id.is_empty() || id.contains(char::is_whitespace)) {
                    return responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled));
                }
                let request_id = uuid::Uuid::new_v4().to_string();
                let (answer, receive) = oneshot::channel();
                permission_pending.lock().expect("pending mutex poisoned").insert(request_id.clone(), Pending { options:ids, answer });
                let sent = permission_updates.send(AgentEvent::Permission {
                    run_id:permission_run.clone(),request_id:request_id.clone(),
                    title:request.tool_call.fields.title.unwrap_or_else(|| "Agent action".into()), options:choices,
                }).await.is_ok();
                // Release SDK dispatch while a human decides; otherwise cancellation,
                // other requests and session updates cannot be processed.
                let permission_pending = permission_pending.clone();
                connection.spawn(async move {
                    let selected = if sent { tokio::time::timeout(approval_ttl,receive).await.ok().and_then(Result::ok).flatten() } else { None };
                    permission_pending.lock().expect("pending mutex poisoned").remove(&request_id);
                    let outcome = selected.map(|id|RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))).unwrap_or(RequestPermissionOutcome::Cancelled);
                    responder.respond(RequestPermissionResponse::new(outcome))
                })
            }, agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            let init = tokio::time::timeout(Duration::from_secs(30), connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task()).await.map_err(|_|error("ACP initialize timed out"))??;
            if init.protocol_version != ProtocolVersion::V1 { return Err(error("agent selected an unsupported protocol")); }
            let (session_id,modes) = if let Some(id) = run.session_id {
                let modes=if init.agent_capabilities.load_session {
                    tokio::time::timeout(Duration::from_secs(30), connection.send_request(LoadSessionRequest::new(id.clone(),harness.workspace.clone()).mcp_servers(mcp_servers.clone())).block_task()).await.map_err(|_|error("ACP load timed out"))??.modes
                } else if init.agent_capabilities.session_capabilities.resume.is_some() {
                    tokio::time::timeout(Duration::from_secs(30), connection.send_request(ResumeSessionRequest::new(id.clone(),harness.workspace.clone()).mcp_servers(mcp_servers.clone())).block_task()).await.map_err(|_|error("ACP resume timed out"))??.modes
                } else {return Err(error("agent cannot resume; refusing to silently lose context"));};
                (SessionId::new(id),modes)
            } else {
                let created = tokio::time::timeout(Duration::from_secs(30), connection.send_request(NewSessionRequest::new(harness.workspace.clone()).mcp_servers(mcp_servers.clone())).block_task()).await.map_err(|_|error("ACP session creation timed out"))??;
                (created.session_id,created.modes)
            };
            if let Some(mode) = harness.mode {
                let modes = modes.ok_or_else(||error("agent must advertise the required permission mode"))?;
                if !modes.available_modes.iter().any(|m|m.id.to_string() == mode) { return Err(error("required permission mode is unavailable")); }
                tokio::time::timeout(Duration::from_secs(30),connection.send_request(SetSessionModeRequest::new(session_id.clone(),mode)).block_task()).await.map_err(|_|error("ACP mode change timed out"))??;
            }
            *expected_session.lock().expect("session mutex poisoned") = Some(session_id.to_string());
            updates.send(AgentEvent::SessionReady {run_id:run.id.clone(),session_id:session_id.to_string()}).await.map_err(|_|error("controller closed"))?;
            let mut next_prompt = run.prompt.clone();
            let mut commands_open = true;
            let status = 'turns: loop {
                active.store(true,Ordering::SeqCst);
                let (responses, mut prompt_responses) = mpsc::unbounded_channel();
                let mut generation = 0_u64;
                submit_prompt(&connection,session_id.clone(),next_prompt,generation,responses.clone())?;
                let mut stopping = false;
                let mut steering: Vec<(String,String)> = vec![];
                let mut concurrent_inputs = Vec::new();
                let mut interjections = FuturesUnordered::new();
                let mut primary_outcome = None;
                let mut needs_native_drain = false;
                let mut drain = None;
                let stop_deadline = tokio::time::sleep(Duration::from_secs(5));
                tokio::pin!(stop_deadline);
                let outcome = loop {
                    if let Some(outcome) = primary_outcome
                        && interjections.is_empty() {
                            if needs_native_drain {
                                if drain.is_none() {
                                    drain = Some(Box::pin(drain_grok(connection.clone(), session_id.clone())));
                                }
                            } else { break outcome; }
                    }
                    tokio::select! {
                        Some((response_generation,response)) = prompt_responses.recv(), if primary_outcome.is_none() => {
                            if response_generation != generation { continue; }
                            primary_outcome = Some(prompt_status(response?));
                            for event_id in std::mem::take(&mut concurrent_inputs) {
                                updates.send(AgentEvent::Steered {run_id:run.id.clone(),event_id}).await.map_err(|_|error("controller closed"))?;
                            }
                        }
                        Some(result) = interjections.next(), if !interjections.is_empty() => {
                            let event_id = result?;
                            needs_native_drain = true;
                            drain = None;
                            updates.send(AgentEvent::Steered {run_id:run.id.clone(),event_id}).await.map_err(|_|error("controller closed"))?;
                        }
                        result = async { drain.as_mut().expect("drain present").await }, if drain.is_some() => {
                            result?;
                            needs_native_drain = false;
                            drain = None;
                        }
                        _ = mode_fault_rx.recv() => { return Err(error("agent changed the required permission mode during a turn")); }
                        _ = &mut stop_deadline, if stopping => { break RunStatus::Interrupted; }
                        command = commands.recv(), if commands_open => match command {
                            Some(Command::Decide {request_id,option_id}) => {
                                let mut map = pending.lock().expect("pending mutex poisoned");
                                if map.get(&request_id).is_some_and(|entry|option_id.as_ref().is_none_or(|id|entry.options.contains(id)))
                                    && let Some(entry) = map.remove(&request_id) {
                                    let _ = entry.answer.send(option_id);
                                }
                            }
                            Some(Command::Steer {event_id,prompt:text}) if !stopping => {
                                drain = None;
                                match harness.steering {
                                    Steering::AfterTurn => steering.push((event_id,text)),
                                    Steering::ConcurrentPrompt => {
                                        // Codex resolves the newest prompt at turn completion;
                                        // superseded requests may never resolve. Keep their
                                        // callbacks alive: dropping an SDK response future
                                        // would send $/cancel_request and interrupt the tool.
                                        concurrent_inputs.push(event_id);
                                        primary_outcome = None;
                                        generation += 1;
                                        submit_prompt(&connection,session_id.clone(),text,generation,responses.clone())?;
                                    },
                                    Steering::GrokInterject => interjections.push(interject_grok(connection.clone(),session_id.clone(),event_id,text)),
                                }
                            }
                            Some(Command::Steer {..}) => {},
                            Some(Command::Cancel) | None => {
                                steering.clear();
                                if !stopping {
                                    stopping = true;
                                    active.store(false,Ordering::SeqCst);
                                    stop_deadline.as_mut().reset(tokio::time::Instant::now()+Duration::from_secs(5));
                                    cancel_pending(&pending);
                                    connection.send_notification(CancelNotification::new(session_id.clone()))?;
                                }
                                if commands.is_closed() { commands_open = false; }
                            }
                        }
                    }
                };
                if !stopping && !steering.is_empty() && outcome == RunStatus::Completed {
                    next_prompt = steering.iter().map(|(_,p)|p.as_str()).collect::<Vec<_>>().join("\n");
                    for (event_id,_) in steering {
                        updates.send(AgentEvent::Steered {run_id:run.id.clone(),event_id}).await.map_err(|_|error("controller closed"))?;
                    }
                    continue 'turns;
                }
                break if stopping && outcome == RunStatus::Completed {RunStatus::Cancelled} else {outcome};
            };
            active.store(false,Ordering::SeqCst);
            cancel_pending(&pending);
            updates.send(AgentEvent::Finished {run_id:run.id,status}).await.map_err(|_|error("controller closed"))?;
            Ok(())
        }).await;
    cancel_pending(&cleanup_pending);
    if result.is_err() {
        // Keep backend stderr/error data off Matrix; it may include credentials or local paths.
        let _ = completion_updates
            .send(AgentEvent::Finished {
                run_id: completion_run,
                status: RunStatus::Failed,
            })
            .await;
    }
    result.map_err(Into::into)
}

fn prompt_status(response: PromptResponse) -> RunStatus {
    match response.stop_reason {
        StopReason::EndTurn => RunStatus::Completed,
        StopReason::Cancelled => RunStatus::Cancelled,
        _ => RunStatus::Failed,
    }
}

type PromptResult = (u64, agent_client_protocol::Result<PromptResponse>);

fn submit_prompt(
    connection: &ConnectionTo<Agent>,
    session: SessionId,
    text: String,
    generation: u64,
    responses: mpsc::UnboundedSender<PromptResult>,
) -> agent_client_protocol::Result<()> {
    connection
        .send_request(PromptRequest::new(
            session,
            vec![ContentBlock::Text(TextContent::new(text))],
        ))
        .on_receiving_result(async move |result| {
            // A superseded response can arrive after its turn driver has ended.
            let _ = responses.send((generation, result));
            Ok(())
        })
}

async fn grok_request(
    connection: &ConnectionTo<Agent>,
    method: &str,
    params: serde_json::Value,
) -> agent_client_protocol::Result<serde_json::Value> {
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        connection
            .send_request(UntypedMessage::new(method, params)?)
            .block_task(),
    )
    .await
    .map_err(|_| error("Grok extension request timed out"))??;
    if response.get("error").is_some_and(|e| !e.is_null()) {
        return Err(error("Grok extension rejected request"));
    }
    response
        .get("result")
        .cloned()
        .filter(|r| !r.is_null())
        .ok_or_else(|| error("Grok extension returned no result"))
}

async fn interject_grok(
    connection: ConnectionTo<Agent>,
    session: SessionId,
    event_id: String,
    text: String,
) -> agent_client_protocol::Result<String> {
    let response = grok_request(
        &connection,
        "_x.ai/interject",
        serde_json::json!({"sessionId":session,"text":text,"interjectionId":event_id}),
    )
    .await?;
    if response.get("status").and_then(|s| s.as_str()) != Some("queued") {
        return Err(error("Grok did not acknowledge interjection"));
    }
    Ok(event_id)
}

/// An interjection racing the end of a Grok turn becomes a provider-owned follow-up.
/// Keep receiving its output and permissions until that session is actually idle.
/// The actor request is a mailbox barrier after the accepted interjection; the
/// original prompt response alone is NOT proof that the follow-up has finished.
async fn drain_grok(
    connection: ConnectionTo<Agent>,
    session: SessionId,
) -> agent_client_protocol::Result<()> {
    loop {
        grok_request(
            &connection,
            "_x.ai/session/info",
            serde_json::json!({"sessionId":session}),
        )
        .await?;
        let response =
            grok_request(&connection, "_x.ai/sessions/list", serde_json::json!({})).await?;
        let row = response
            .get("sessions")
            .and_then(|s| s.as_array())
            .and_then(|rows| {
                rows.iter().find(|r| {
                    r.get("sessionId").and_then(|s| s.as_str())
                        == Some(session.to_string().as_str())
                })
            })
            .ok_or_else(|| error("Grok session absent from active session roster"))?;
        match row.get("activity").and_then(|v| v.as_str()) {
            Some("idle" | "completed") => return Ok(()),
            Some("working" | "needs_input") => {}
            _ => return Err(error("Grok session stopped before interjection completed")),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Live stdio transport, also used by the explicit ACP-only doctor command.
/// An environment allowlist is hygiene; OS-level worker isolation is still required.
pub struct ScopedProcess(pub HarnessConfig);

impl ConnectTo<Client> for ScopedProcess {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> agent_client_protocol::Result<()> {
        let mut child = spawn_scoped(&self.0)?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| error("worker stdin missing"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| error("worker stdout missing"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| error("worker stderr missing"))?;
        let _guard = ChildGuard(child);
        let drain = async move {
            let _ = futures::io::copy(stderr, &mut futures::io::sink()).await;
            futures::future::pending::<()>().await;
        };
        tokio::select! {
            result = client.connect_to(agent_client_protocol::ByteStreams::new(input,output)) => result,
            _ = drain => unreachable!("stderr drainer never completes"),
        }
    }
}

fn spawn_scoped(harness: &HarnessConfig) -> agent_client_protocol::Result<async_process::Child> {
    use std::process::Stdio;
    let mut command = std::process::Command::new(&harness.program);
    command
        .args(&harness.args)
        .env_clear()
        .envs(&harness.env)
        // ACP receives the configured workspace through session/new or session/load.
        // Starting from / also permits an operator-supplied run-as launcher.
        .current_dir("/");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = async_process::Command::from(command)
        // async-process resets inherited stdio configuration on spawn.
        // Set pipes on its wrapper, after converting the std command.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .reap_on_drop(true)
        .spawn()
        .map_err(|_| error("cannot start scoped ACP worker"))?;
    Ok(child)
}

struct ChildGuard(async_process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pid) = rustix::process::Pid::from_raw(self.0.id() as i32) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
        }
        let _ = self.0.kill();
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    use futures::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn real_child_stdio_survives_command_conversion() {
        let mut harness = crate::config::Config::parse(include_str!("../config/example.toml"))
            .unwrap()
            .harness;
        harness.program = "/bin/cat".into();
        harness.args.clear();
        let mut child = spawn_scoped(&harness).unwrap();
        let mut input = child.stdin.take().expect("piped stdin");
        let mut output = child.stdout.take().expect("piped stdout");
        assert!(child.stderr.is_some(), "piped stderr");
        input.write_all(b"stdio regression\n").await.unwrap();
        drop(input);
        let mut text = String::new();
        tokio::time::timeout(Duration::from_secs(5), output.read_to_string(&mut text))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(text, "stdio regression\n");
        assert!(child.status().await.unwrap().success());
    }
}
