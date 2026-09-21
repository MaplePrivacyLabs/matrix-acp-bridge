//! Deterministic protocol fixture. No AI, tools, subprocesses, credentials or network.

use std::sync::{Arc, Mutex};

use agent_client_protocol::{Agent, Client, DynConnectTo, schema::v1::*};
use serde_json::json;
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug)]
pub enum Behavior {
    Reply,
    Approval,
    WaitForCancel,
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
}

#[derive(Clone)]
pub struct FixtureAgent {
    pub behavior: Behavior,
    pub observations: Arc<Mutex<Observations>>,
}

impl FixtureAgent {
    pub fn new(behavior: Behavior) -> Self {
        Self {
            behavior,
            observations: Arc::default(),
        }
    }

    pub fn transport(&self) -> DynConnectTo<Client> {
        let obs_init = self.observations.clone();
        let obs_new = self.observations.clone();
        let obs_load = self.observations.clone();
        let obs_prompt = self.observations.clone();
        let behavior = self.behavior;
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
                let response: NewSessionResponse = serde_json::from_value(json!({"sessionId":format!("fixture-{}",obs.new_sessions),"modes":modes()})).expect("valid fixture");
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
                cancelled.notify_one();
                Ok(())
            },agent_client_protocol::on_receive_notification!())
            .on_receive_request(async move |request: PromptRequest,responder,cx| {
                let obs_prompt = obs_prompt.clone();
                let cancel_prompt = cancel_prompt.clone();
                let prompt_cx = cx.clone();
                cx.spawn(async move {
                let cx = prompt_cx;
                let text = request.prompt.iter().filter_map(|block|match block {ContentBlock::Text(t)=>Some(t.text.as_str()),_=>None}).collect::<Vec<_>>().join("\n");
                obs_prompt.lock().expect("fixture lock").prompts.push(text.clone());
                let session = request.session_id.clone();
                if matches!(behavior,Behavior::WaitForCancel) {
                    cancel_prompt.notified().await;
                    return responder.respond(PromptResponse::new(StopReason::Cancelled));
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
            },agent_client_protocol::on_receive_request!()))
    }
}

fn modes() -> serde_json::Value {
    json!({"currentModeId":"read-only","availableModes":[{"id":"read-only","name":"Read only"}]})
}
