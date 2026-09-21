//! Deterministic protocol fixture. No AI, tools, subprocesses, credentials or network.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use agent_client_protocol::{Agent, Client, DynConnectTo, UntypedMessage, schema::v1::*};
use serde_json::json;
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug)]
pub enum Behavior {
    Reply,
    Approval,
    WaitForCancel,
    SteerThenReply,
    CodexSteer,
    GrokInterject,
    GrokLate,
    NoModes,
    RejectMode,
    WrongSession,
    ChangeMode,
}

#[derive(Default, Debug)]
pub struct Observations {
    pub prompts: Vec<String>,
    pub loaded: Vec<String>,
    pub new_sessions: usize,
    pub decisions: Vec<String>,
    pub client_tools_disabled: bool,
    pub cancellations: usize,
    pub interjections: Vec<String>,
    pub tool_finished: bool,
    pub roster_checks: usize,
}

#[derive(Clone)]
pub struct FixtureAgent {
    pub behavior: Behavior,
    pub observations: Arc<Mutex<Observations>>,
    pub release_tool: Arc<Notify>,
    pub finish_native: Arc<AtomicBool>,
}

impl FixtureAgent {
    pub fn new(behavior: Behavior) -> Self {
        Self {
            behavior,
            observations: Arc::default(),
            release_tool: Arc::default(),
            finish_native: Arc::default(),
        }
    }

    pub fn transport(&self) -> DynConnectTo<Client> {
        let obs_init = self.observations.clone();
        let obs_new = self.observations.clone();
        let obs_load = self.observations.clone();
        let obs_prompt = self.observations.clone();
        let behavior = self.behavior;
        let release_tool = self.release_tool.clone();
        let obs_cancel = self.observations.clone();
        let obs_ext = self.observations.clone();
        let finish_native = self.finish_native.clone();
        let cancelled = Arc::new(Notify::new());
        let cancel_prompt = cancelled.clone();
        DynConnectTo::new(Agent.builder()
            .on_receive_request(async move |request: InitializeRequest,responder,_cx| {
                let capabilities = serde_json::to_value(&request.client_capabilities).expect("capabilities serialize");
                let disabled = capabilities.pointer("/fs/readTextFile").and_then(|v|v.as_bool()) != Some(true)
                    && capabilities.pointer("/fs/writeTextFile").and_then(|v|v.as_bool()) != Some(true)
                    && capabilities.get("terminal").and_then(|v|v.as_bool()) != Some(true);
                obs_init.lock().expect("fixture lock").client_tools_disabled = disabled;
                responder.respond(InitializeResponse::new(request.protocol_version).agent_capabilities(AgentCapabilities::new().load_session(true)))
            },agent_client_protocol::on_receive_request!())
            .on_receive_request(async move |_request: NewSessionRequest,responder,_cx| {
                let mut obs = obs_new.lock().expect("fixture lock");
                obs.new_sessions += 1;
                let mut value=json!({"sessionId":format!("fixture-{}",obs.new_sessions)});
                if !matches!(behavior,Behavior::NoModes) { value["modes"]=modes(); }
                let response: NewSessionResponse = serde_json::from_value(value).expect("valid fixture");
                responder.respond(response)
            },agent_client_protocol::on_receive_request!())
            .on_receive_request(async move |request: LoadSessionRequest,responder,cx| {
                obs_load.lock().expect("fixture lock").loaded.push(request.session_id.to_string());
                cx.send_notification(SessionNotification::new(request.session_id,SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new("OLD HISTORY MUST NOT BE REPOSTED"))))))?;
                responder.respond(serde_json::from_value::<LoadSessionResponse>(json!({"modes":modes()})).expect("valid fixture"))
            },agent_client_protocol::on_receive_request!())
            .on_receive_request(async move |_request: SetSessionModeRequest,responder,_cx| {
                if matches!(behavior,Behavior::RejectMode) { responder.respond_with_error(agent_client_protocol::Error::internal_error()) }
                else { responder.respond(SetSessionModeResponse::default()) }
            },agent_client_protocol::on_receive_request!())
            .on_receive_notification(async move |_notification: CancelNotification,_cx| {
                obs_cancel.lock().unwrap().cancellations += 1;
                cancelled.notify_one();
                Ok(())
            },agent_client_protocol::on_receive_notification!())
            .on_receive_request(async move |request: PromptRequest,responder,cx| {
                let obs_prompt = obs_prompt.clone();
                let cancel_prompt = cancel_prompt.clone();
                let prompt_cx = cx.clone();
                let release_tool = release_tool.clone();
                cx.spawn(async move {
                let cx = prompt_cx;
                let text = request.prompt.iter().filter_map(|block|match block {ContentBlock::Text(t)=>Some(t.text.as_str()),_=>None}).collect::<Vec<_>>().join("\n");
                obs_prompt.lock().expect("fixture lock").prompts.push(text.clone());
                let session = request.session_id.clone();
                if matches!(behavior,Behavior::WaitForCancel) {
                    cancel_prompt.notified().await;
                    return responder.respond(PromptResponse::new(StopReason::Cancelled));
                }
                if matches!(behavior,Behavior::SteerThenReply | Behavior::CodexSteer | Behavior::GrokInterject | Behavior::GrokLate) && obs_prompt.lock().unwrap().prompts.len()==1 {
                    // A deterministic running tool: only the test can release it.
                    cx.send_notification(SessionNotification::new(session.clone(),SessionUpdate::ToolCall(ToolCall::new("held-tool","Fixture running tool"))))?;
                    tokio::select! {
                        _ = release_tool.notified() => { obs_prompt.lock().unwrap().tool_finished=true; },
                        _ = cancel_prompt.notified() => { return responder.respond(PromptResponse::new(StopReason::Cancelled)); }
                    }
                }
                if matches!(behavior,Behavior::CodexSteer) {
                    if text == "original work" {
                        if responder.cancellation().is_cancelled() {
                            obs_prompt.lock().unwrap().cancellations += 1;
                        }
                        futures::future::pending::<()>().await;
                    } else {
                        while !obs_prompt.lock().unwrap().tool_finished {tokio::task::yield_now().await;}
                    }
                }
                if matches!(behavior,Behavior::ChangeMode) {
                    cx.send_notification(SessionNotification::new(session.clone(),SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new("full-access"))))?;
                    futures::future::pending::<()>().await;
                }
                if matches!(behavior,Behavior::Approval) {
                    let permission: RequestPermissionRequest = serde_json::from_value(json!({
                        "sessionId":session,"toolCall":{"toolCallId":"fixture-tool","title":"Edit a fixture file"},
                        "options":[{"optionId":"permit-once","name":"Allow once","kind":"allow_once"},{"optionId":"deny-once","name":"Deny","kind":"reject_once"}]
                    })).expect("valid fixture");
                    let decision = cx.send_request(permission).block_task().await?;
                    obs_prompt.lock().expect("fixture lock").decisions.push(serde_json::to_string(&decision.outcome).expect("decision serializes"));
                }
                let emitted_session = if matches!(behavior,Behavior::WrongSession) {SessionId::new("other-session")} else {session};
                cx.send_notification(SessionNotification::new(emitted_session.clone(),SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(TextContent::new("PRIVATE FIXTURE REASONING"))))))?;
                for chunk in ["Fixture response: ".to_string(),text] {
                    cx.send_notification(SessionNotification::new(emitted_session.clone(),SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(chunk))))))?;
                }
                responder.respond(PromptResponse::new(StopReason::EndTurn))
                })
            },agent_client_protocol::on_receive_request!())
            .on_receive_request(async move |request: UntypedMessage,responder,cx| {
                if !matches!(behavior,Behavior::GrokInterject | Behavior::GrokLate) { return responder.respond_with_error(agent_client_protocol::Error::method_not_found()); }
                let value=match request.method.as_str() {
                    "_x.ai/interject" => {
                        obs_ext.lock().unwrap().interjections.push(request.params["text"].as_str().unwrap().to_owned());
                        json!({"status":"queued"})
                    },
                    "_x.ai/session/info" => json!({}),
                    "_x.ai/sessions/list" => {
                        let mut obs=obs_ext.lock().unwrap();
                        obs.roster_checks += 1;
                        let idle=obs.tool_finished && (!matches!(behavior,Behavior::GrokLate) || finish_native.load(Ordering::SeqCst));
                        if idle && matches!(behavior,Behavior::GrokLate) {
                            cx.send_notification(SessionNotification::new("fixture-1",SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new("Late follow-up response"))))))?;
                        }
                        json!({"sessions":[{"sessionId":"fixture-1","activity":if idle {"idle"} else {"working"}}]})
                    },
                    _ => return responder.respond_with_error(agent_client_protocol::Error::method_not_found()),
                };
                responder.respond(json!({"result":value}))
            },agent_client_protocol::on_receive_request!()))
    }
}

fn modes() -> serde_json::Value {
    json!({"currentModeId":"read-only","availableModes":[{"id":"read-only","name":"Read only"}]})
}
