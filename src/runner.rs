use std::{collections::BTreeMap, sync::Arc, time::Duration};

use agent_client_protocol::{Client, DynConnectTo};
use anyhow::Result;
use futures::FutureExt;
use tokio::sync::mpsc;

use crate::{
    acp::{self, Command},
    config::HarnessConfig,
    core::Bridge,
    model::*,
};

pub type TransportFactory = Arc<dyn Fn(&HarnessConfig) -> DynConnectTo<Client> + Send + Sync>;

/// Coordinates concurrent conversations, with one run at a time in each conversation.
/// Admission enforces the configured worker concurrency limit.
pub struct Runner {
    pub bridge: Bridge,
    factory: TransportFactory,
    mcp_servers: Vec<agent_client_protocol::schema::v1::McpServer>,
    controls: BTreeMap<String, mpsc::Sender<Command>>,
    tasks: tokio::task::JoinSet<()>,
    updates: mpsc::Sender<AgentEvent>,
    received: mpsc::Receiver<AgentEvent>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Runner {
    pub fn new(bridge: Bridge, factory: TransportFactory) -> Self {
        let (updates, received) = mpsc::channel(128);
        Self {
            bridge,
            factory,
            mcp_servers: vec![],
            controls: BTreeMap::new(),
            tasks: tokio::task::JoinSet::new(),
            updates,
            received,
            clock: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock before Unix epoch")
                    .as_secs() as i64
            }),
        }
    }

    pub fn with_mcp_servers(
        mut self,
        servers: Vec<agent_client_protocol::schema::v1::McpServer>,
    ) -> Self {
        self.mcp_servers = servers;
        self
    }

    /// Deterministic clocks for offline fixtures; production uses wall time on arrival.
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    pub async fn ingest(
        &mut self,
        event: &Incoming,
        room: &RoomSnapshot,
        now: i64,
    ) -> Result<Handled> {
        self.ingest_with_context(event, room, None, now).await
    }

    pub async fn ingest_with_context(
        &mut self,
        event: &Incoming,
        room: &RoomSnapshot,
        context: Option<&[Incoming]>,
        now: i64,
    ) -> Result<Handled> {
        while self.try_update()?.is_some() {}
        self.reconcile_room(&event.room_id, room, now).await?;
        // Deliver an older completion-race instruction before accepting a newer
        // message in that conversation. Commands (especially stop) go first.
        if !event.body.trim().starts_with("!bridge ") {
            let pending = self.bridge.resume_pending_steers(now)?;
            self.apply(&pending, now).await?;
        }
        let handled = self.bridge.handle_with_context(event, room, context, now)?;
        self.apply(&handled.effects, now).await?;
        Ok(handled)
    }

    async fn apply(&mut self, effects: &[Effect], now: i64) -> Result<()> {
        for effect in effects {
            match effect {
                Effect::Start { run_id } => {
                    let run = self.bridge.started(run_id)?;
                    let harness = self.bridge.config.harness.clone();
                    let transport = (self.factory)(&harness);
                    let mcp_servers = self.mcp_servers.clone();
                    let (send, receive) = mpsc::channel(32);
                    self.controls.insert(run_id.clone(), send);
                    let updates = self.updates.clone();
                    let failure_updates = updates.clone();
                    let failure_id = run_id.clone();
                    let ttl = Duration::from_secs(self.bridge.config.approval_ttl_seconds);
                    self.tasks.spawn(async move {
                        let result = std::panic::AssertUnwindSafe(acp::execute(
                            transport,
                            harness,
                            run,
                            mcp_servers,
                            ttl,
                            receive,
                            updates,
                        ))
                        .catch_unwind()
                        .await;
                        // execute() publishes ordinary protocol failures itself.
                        // Only synthesize a terminal event if the task panicked.
                        if result.is_err() {
                            let _ = failure_updates
                                .send(AgentEvent::Finished {
                                    run_id: failure_id,
                                    status: RunStatus::Failed,
                                })
                                .await;
                        }
                    });
                }
                Effect::Cancel { run_id } => {
                    if let Some(control) = self.controls.get(run_id) {
                        let _ = control.send(Command::Cancel).await;
                    } else {
                        self.bridge.agent_event(
                            AgentEvent::Finished {
                                run_id: run_id.clone(),
                                status: RunStatus::Interrupted,
                            },
                            now,
                        )?;
                    }
                }
                Effect::Steer {
                    run_id,
                    event_id,
                    prompt,
                } => {
                    if let Some(control) = self.controls.get(run_id) {
                        // A terminal-turn race is recovered from the durable steer row.
                        let _ = control
                            .send(Command::Steer {
                                event_id: event_id.clone(),
                                prompt: prompt.clone(),
                            })
                            .await;
                    }
                }
                Effect::Decide {
                    run_id,
                    request_id,
                    option_id,
                } => {
                    if let Some(control) = self.controls.get(run_id) {
                        let _ = control
                            .send(Command::Decide {
                                request_id: request_id.clone(),
                                option_id: option_id.clone(),
                            })
                            .await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Wait for one worker event. External drivers select this against input and timers.
    pub async fn next_update(&mut self) -> Result<AgentEvent> {
        let event = self
            .received
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("worker update channel closed"))?;
        self.record_update(event)
    }

    /// Drain worker output between short Matrix polls without blocking for a turn.
    pub fn try_update(&mut self) -> Result<Option<AgentEvent>> {
        match self.received.try_recv() {
            Ok(event) => self.record_update(event).map(Some),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                anyhow::bail!("worker update channel closed")
            }
        }
    }

    fn record_update(&mut self, event: AgentEvent) -> Result<AgentEvent> {
        self.bridge.agent_event(event.clone(), (self.clock)())?;
        if let AgentEvent::Finished { run_id, .. } = &event {
            self.controls.remove(run_id);
        }
        while self.tasks.try_join_next().is_some() {}
        Ok(event)
    }

    pub async fn tick(&mut self, now: i64) -> Result<()> {
        let effects = self.bridge.expire_approvals(now)?;
        self.apply(&effects, now).await?;
        let effects = self.bridge.resume_pending_steers(now)?;
        self.apply(&effects, now).await?;
        let effects = self.bridge.automatic_approvals(now)?;
        self.apply(&effects, now).await?;
        self.bridge.flush_progress(now)?;
        Ok(())
    }

    pub fn active_count(&self) -> usize {
        self.controls.len()
    }

    pub async fn reconcile_room(&mut self, id: &str, room: &RoomSnapshot, now: i64) -> Result<()> {
        let effects = self.bridge.reconcile_room(id, room)?;
        self.apply(&effects, now).await
    }

    pub async fn shutdown(&mut self, now: i64) -> Result<()> {
        for sender in self.controls.values() {
            let _ = sender.send(Command::Cancel).await;
        }
        let deadline = tokio::time::sleep(Duration::from_secs(6));
        tokio::pin!(deadline);
        while !self.controls.is_empty() {
            tokio::select! {
                _ = &mut deadline => break,
                result = self.next_update() => { result?; }
            }
        }
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
        self.bridge.store.recover(now)?;
        self.controls.clear();
        Ok(())
    }
}
