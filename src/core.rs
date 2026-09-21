use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};

use crate::{
    config::{Config, ConversationMode, OperatorTrust, ToolApproval, digest},
    model::*,
    store::{Store, enqueue, enqueue_reaction},
};

/// Single-writer durable state machine. Transport and ACP execution are external effects.
pub struct Bridge {
    pub config: Config,
    pub store: Store,
}

impl Bridge {
    pub fn new(config: Config, store: Store) -> Result<Self> {
        config.validate()?;
        Ok(Self { config, store })
    }

    pub fn authorized(&self, event: &Incoming, room: &RoomSnapshot) -> bool {
        let Some(policy) = self.config.room(&event.room_id) else {
            return false;
        };
        event.encrypted
            && (event.verified_device
                || (event.known_sender_device && policy.operator_trust == OperatorTrust::Account))
            && room.joined
            && room.encrypted
            && policy.operators.contains(&event.sender)
            && room.members.contains(&event.sender)
            && room.members.contains(&self.config.bot_user_id)
            && policy.permits_members(&room.members)
            && event.sender != self.config.bot_user_id
    }

    fn conversation(&self, event: &Incoming) -> Conversation {
        let policy = self
            .config
            .room(&event.room_id)
            .expect("checked room policy");
        let root = match policy.conversation {
            ConversationMode::Room => None,
            ConversationMode::Thread => Some(
                event
                    .thread_root
                    .clone()
                    .unwrap_or_else(|| event.event_id.clone()),
            ),
        };
        let key = digest(
            &serde_json::to_vec(&(&self.config.bot_user_id, &event.room_id, &root))
                .expect("strings serialize"),
        );
        Conversation {
            key,
            room_id: event.room_id.clone(),
            thread_root: root,
        }
    }

    fn admission(
        &self,
        event: &Incoming,
        room: &RoomSnapshot,
    ) -> Result<Result<Conversation, Disposition>> {
        if !self.authorized(event, room) {
            return Ok(Err(Disposition::Denied));
        }
        let conversation = self.conversation(event);
        let policy_hash = self.config.binding_fingerprint(&event.room_id);
        let audience_hash = self
            .config
            .room(&event.room_id)
            .expect("admitted room")
            .audience_binding(&room.members);
        let binding: Option<(String, String)> = self
            .store
            .db
            .query_row(
                "SELECT policy,audience FROM conversations WHERE key=?1",
                [&conversation.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // Changes in audience or authority require an explicit fresh conversation.
        if binding
            .as_ref()
            .is_some_and(|(p, a)| p != &policy_hash || a != &audience_hash)
        {
            return Ok(Err(Disposition::Denied));
        }
        let reply_to_bot = if let Some(reply) = &event.reply_to {
            self.store.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM outbox WHERE delivered_event=?1 AND conversation=?2)",
                params![reply, conversation.key],
                |r| r.get::<_, bool>(0),
            )?
        } else {
            false
        };
        let command = event.body.trim().starts_with("!bridge ");
        let in_bound_thread = event.thread_root.is_some() && binding.is_some();
        if !command
            && !event.mentions.contains(&self.config.bot_user_id)
            && !in_bound_thread
            && !reply_to_bot
        {
            return Ok(Err(Disposition::Ignored));
        }
        // Commands cannot create a conversation or discover unrelated stored sessions.
        if command && binding.is_none() {
            return Ok(Err(Disposition::Ignored));
        }

        Ok(Ok(conversation))
    }

    /// Fetch history only for a new, authorized work request, never for a reader's trigger.
    pub fn needs_context(&self, event: &Incoming, room: &RoomSnapshot) -> Result<bool> {
        if event.body.trim().starts_with("!bridge ") {
            return Ok(false);
        }
        let Ok(_) = self.admission(event, room)? else {
            return Ok(false);
        };
        let seen: bool = self.store.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM inbox WHERE room=?1 AND event=?2)",
            params![event.room_id, event.event_id],
            |r| r.get(0),
        )?;
        Ok(!seen)
    }

    pub fn handle(&mut self, event: &Incoming, room: &RoomSnapshot, now: i64) -> Result<Handled> {
        self.handle_with_context(event, room, None, now)
    }

    pub fn handle_with_context(
        &mut self,
        event: &Incoming,
        room: &RoomSnapshot,
        context: Option<&[Incoming]>,
        now: i64,
    ) -> Result<Handled> {
        let conversation = match self.admission(event, room)? {
            Ok(conversation) => conversation,
            Err(disposition) => return Ok(Handled::new(disposition)),
        };
        let policy_hash = self.config.binding_fingerprint(&event.room_id);
        let audience_hash = self
            .config
            .room(&event.room_id)
            .expect("admitted room")
            .audience_binding(&room.members);
        let command = event.body.trim().starts_with("!bridge ");
        let prompt = if let Some(context) = context {
            crate::context::prompt(&self.config, event, context)?
        } else {
            event.body.clone()
        };

        let tx = self.store.db.transaction()?;
        if tx.execute(
            "INSERT OR IGNORE INTO inbox(room,event) VALUES(?1,?2)",
            params![event.room_id, event.event_id],
        )? == 0
        {
            return Ok(Handled::new(Disposition::Duplicate));
        }
        tx.execute("INSERT OR IGNORE INTO conversations(key,room,root,policy,audience) VALUES(?1,?2,?3,?4,?5)", params![conversation.key,conversation.room_id,conversation.thread_root,policy_hash,audience_hash])?;
        let active: Option<(String,String)> = tx.query_row("SELECT id,status FROM runs WHERE conversation=?1 AND status IN ('queued','running','waiting_approval','cancelling')", [&conversation.key], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let mut effects = vec![];
        if command {
            let words: Vec<_> = event.body.split_whitespace().collect();
            match words.as_slice() {
                ["!bridge", "status"] => {
                    let last: Option<(String,String)> = tx.query_row("SELECT id,status FROM runs WHERE conversation=?1 ORDER BY rowid DESC LIMIT 1", [&conversation.key], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                    let text = last
                        .map(|(id, status)| format!("Run {id}: {status}."))
                        .unwrap_or_else(|| "No run in this conversation.".into());
                    enqueue(&tx, &conversation.key, &text, now)?;
                }
                ["!bridge", "stop"] => {
                    tx.execute("UPDATE run_steers SET status='cancelled' WHERE status='pending' AND run IN (SELECT id FROM runs WHERE conversation=?1)", [&conversation.key])?;
                    if let Some((run_id, _)) = active {
                        tx.execute("UPDATE runs SET status='cancelling' WHERE id=?1", [&run_id])?;
                        tx.execute("UPDATE approvals SET consumed=1 WHERE run=?1", [&run_id])?;
                        tx.execute("UPDATE run_steers SET status='cancelled' WHERE run=?1 AND status='pending'", [&run_id])?;
                        // Keep the run active until the ACP worker confirms cancellation.
                        effects.push(Effect::Cancel { run_id });
                        enqueue(
                            &tx,
                            &conversation.key,
                            "Cancellation requested; waiting for the worker to stop.",
                            now,
                        )?;
                    } else {
                        enqueue(&tx, &conversation.key, "No active run.", now)?;
                    }
                }
                ["!bridge", "allow-thread"] => {
                    tx.execute(
                        "INSERT OR REPLACE INTO conversation_permissions VALUES(?1,1)",
                        [&conversation.key],
                    )?;
                    enqueue(
                        &tx,
                        &conversation.key,
                        "Tool requests are now approved automatically in this conversation, including the pending request. The worker's configured access and ACP mode still apply. Use !bridge approvals manual to turn this off.",
                        now,
                    )?;
                }
                ["!bridge", "approvals", "manual"] => {
                    tx.execute(
                        "INSERT OR REPLACE INTO conversation_permissions VALUES(?1,0)",
                        [&conversation.key],
                    )?;
                    enqueue(
                        &tx,
                        &conversation.key,
                        "Individual tool approvals restored for this conversation.",
                        now,
                    )?;
                }
                ["!bridge", "approve", request_id, option_id] => {
                    let permission: Option<(String,String,i64,bool)> = tx.query_row("SELECT a.run,a.options,a.expires,a.consumed FROM approvals a JOIN runs r ON r.id=a.run WHERE a.id=?1 AND r.conversation=?2 AND r.status='waiting_approval'", params![request_id,conversation.key], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
                    if let Some((run_id, options, expires, false)) =
                        permission.filter(|(_, _, expires, _)| *expires > now)
                    {
                        let options: Vec<PermissionOption> = serde_json::from_str(&options)?;
                        if options.iter().any(|o| o.id == *option_id) {
                            tx.execute(
                                "UPDATE approvals SET consumed=1 WHERE id=?1 AND consumed=0",
                                [request_id],
                            )?;
                            let remaining: i64 = tx.query_row(
                                "SELECT COUNT(*) FROM approvals WHERE run=?1 AND consumed=0",
                                [&run_id],
                                |r| r.get(0),
                            )?;
                            if remaining == 0 {
                                tx.execute(
                                    "UPDATE runs SET status='running' WHERE id=?1",
                                    [&run_id],
                                )?;
                            }
                            effects.push(Effect::Decide {
                                run_id,
                                request_id: request_id.to_string(),
                                option_id: Some(option_id.to_string()),
                            });
                            enqueue(&tx, &conversation.key, "Approval response accepted.", now)?;
                        } else {
                            enqueue(
                                &tx,
                                &conversation.key,
                                "That action was not offered; approval remains pending.",
                                now,
                            )?;
                        }
                        let _ = expires;
                    } else {
                        enqueue(
                            &tx,
                            &conversation.key,
                            "Approval is unavailable, expired, or belongs to another conversation.",
                            now,
                        )?;
                    }
                }
                _ => {
                    enqueue(
                        &tx,
                        &conversation.key,
                        "Commands: !bridge status; !bridge stop; !bridge allow-thread; !bridge approvals manual; !bridge approve <request-id> <offered-option-id>.",
                        now,
                    )?;
                }
            }
        } else if let Some((run_id, status)) = active {
            if !event.body.trim().is_empty() {
                tx.execute(
                    "INSERT INTO run_steers VALUES(?1,?2,?3,'pending')",
                    params![event.event_id, run_id, prompt],
                )?;
                enqueue_reaction(&tx, &conversation.key, &event.event_id, "👀", now)?;
                if status != "cancelling" {
                    effects.push(Effect::Steer {
                        run_id,
                        event_id: event.event_id.clone(),
                        prompt,
                    });
                }
            }
        } else if !event.body.trim().is_empty() {
            let count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM runs WHERE status IN ('queued','running','waiting_approval','cancelling')",
                [],
                |r| r.get(0),
            )?;
            if self.config.max_concurrent_runs != 0
                && count >= self.config.max_concurrent_runs as i64
            {
                enqueue(
                    &tx,
                    &conversation.key,
                    "This worker is at its configured concurrency limit. Send the instruction again after a run finishes.",
                    now,
                )?;
                tx.commit()?;
                return Ok(Handled::new(Disposition::Accepted));
            }
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute("INSERT INTO runs(id,conversation,prompt,status,created) VALUES(?1,?2,?3,'queued',?4)", params![id,conversation.key,prompt,now])?;
            tx.execute(
                "INSERT INTO run_inputs(run,event) VALUES(?1,?2)",
                params![id, event.event_id],
            )?;
            enqueue_reaction(&tx, &conversation.key, &event.event_id, "👀", now)?;
            effects.push(Effect::Start { run_id: id });
        }
        tx.commit()?;
        Ok(Handled {
            disposition: Disposition::Accepted,
            effects,
        })
    }

    pub fn started(&mut self, id: &str) -> Result<Run> {
        let run = self
            .store
            .run(id)?
            .ok_or_else(|| anyhow::anyhow!("unknown run"))?;
        ensure!(
            run.status == RunStatus::Queued,
            "run was already dispatched"
        );
        self.store.set_status(id, RunStatus::Running)?;
        Ok(run)
    }

    pub fn agent_event(&mut self, event: AgentEvent, now: i64) -> Result<()> {
        let run_id = match &event {
            AgentEvent::SessionReady { run_id, .. }
            | AgentEvent::Text { run_id, .. }
            | AgentEvent::Tool { run_id, .. }
            | AgentEvent::Permission { run_id, .. }
            | AgentEvent::Steered { run_id, .. }
            | AgentEvent::Finished { run_id, .. } => run_id,
        };
        let Some(run) = self.store.run(run_id)? else {
            return Ok(());
        };
        if !run.status.active() {
            return Ok(());
        }
        if run.status == RunStatus::Cancelling && !matches!(event, AgentEvent::Finished { .. }) {
            return Ok(());
        }
        let automatic = self.automatic_tools(&run.conversation)?;
        let tx = self.store.db.transaction()?;
        match event {
            AgentEvent::SessionReady { session_id, .. } => {
                ensure!(!session_id.is_empty(), "empty ACP session ID");
                if let Some(existing) = run.session_id {
                    ensure!(
                        existing == session_id,
                        "resumed agent returned a different session ID"
                    );
                }
                tx.execute(
                    "UPDATE conversations SET session=?2 WHERE key=?1",
                    params![run.conversation.key, session_id],
                )?;
            }
            AgentEvent::Text { text, .. } => {
                tx.execute(
                    "INSERT INTO output(run,body) VALUES(?1,?2)",
                    params![run.id, text],
                )?;
            }
            AgentEvent::Tool { title, .. } => {
                enqueue(
                    &tx,
                    &run.conversation.key,
                    &format!("Working: {title}"),
                    now,
                )?;
            }
            AgentEvent::Permission {
                request_id,
                title,
                options,
                ..
            } => {
                ensure!(!options.is_empty(), "permission request offered no actions");
                let option_ids: std::collections::BTreeSet<_> =
                    options.iter().map(|o| &o.id).collect();
                ensure!(
                    option_ids.len() == options.len()
                        && options
                            .iter()
                            .all(|o| !o.id.is_empty() && !o.id.contains(char::is_whitespace)),
                    "permission options must have distinct command-safe IDs"
                );
                // A duplicate request must not reset its expiry or overwrite its offered actions.
                if tx.execute(
                    "INSERT OR IGNORE INTO approvals(id,run,options,expires) VALUES(?1,?2,?3,?4)",
                    params![
                        request_id,
                        run.id,
                        serde_json::to_string(&options)?,
                        now.saturating_add(self.config.approval_ttl_seconds as i64)
                    ],
                )? == 1
                {
                    tx.execute(
                        "UPDATE runs SET status='waiting_approval' WHERE id=?1",
                        [&run.id],
                    )?;
                    if automatic && allow_option(&options).is_some() {
                        tx.commit()?;
                        return Ok(());
                    }
                    let choices = options
                        .iter()
                        .map(|o| format!("{}: {} ({})", o.id, o.label, o.kind))
                        .collect::<Vec<_>>()
                        .join("\n");
                    enqueue(
                        &tx,
                        &run.conversation.key,
                        &format!(
                            "Approval requested: {title}\n{choices}\nReply in this conversation: !bridge approve {request_id} <option-id>\nOr approve tools for this conversation: !bridge allow-thread\nExpires in {} seconds.",
                            self.config.approval_ttl_seconds
                        ),
                        now,
                    )?;
                }
            }
            AgentEvent::Steered { event_id, .. } => {
                tx.execute("UPDATE run_steers SET status='delivered' WHERE event=?1 AND run=?2 AND status='pending'", params![event_id,run.id])?;
                tx.execute("UPDATE approvals SET consumed=1 WHERE run=?1", [&run.id])?;
                tx.execute("UPDATE runs SET status='running' WHERE id=?1", [&run.id])?;
            }
            AgentEvent::Finished { status, .. } => {
                ensure!(!status.active(), "completion must have a terminal status");
                flush_run(&tx, &run, now)?;
                tx.execute(
                    "UPDATE runs SET status=?2 WHERE id=?1",
                    params![run.id, status.as_str()],
                )?;
                tx.execute("UPDATE approvals SET consumed=1 WHERE run=?1", [&run.id])?;
                let input: Option<String> = tx
                    .query_row(
                        "SELECT event FROM run_inputs WHERE run=?1",
                        [&run.id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(event) = input.or(run.conversation.thread_root.clone()) {
                    let emoji = match status {
                        RunStatus::Completed => "✅",
                        RunStatus::Cancelled => "🛑",
                        RunStatus::Interrupted => "⚠️",
                        _ => "❌",
                    };
                    enqueue_reaction(&tx, &run.conversation.key, &event, emoji, now)?;
                    let steered: Vec<String> = {
                        let mut stmt = tx.prepare("SELECT event FROM run_steers WHERE run=?1 AND status='delivered' AND event<>?2")?;
                        stmt.query_map(params![run.id, event], |r| r.get(0))?
                            .collect::<rusqlite::Result<_>>()?
                    };
                    for event in steered {
                        enqueue_reaction(&tx, &run.conversation.key, &event, emoji, now)?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn automatic_tools(&self, conversation: &Conversation) -> Result<bool> {
        let override_policy: Option<bool> = self
            .store
            .db
            .query_row(
                "SELECT automatic FROM conversation_permissions WHERE conversation=?1",
                [&conversation.key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(override_policy.unwrap_or_else(|| {
            self.config
                .room(&conversation.room_id)
                .is_some_and(|p| p.tool_approval == ToolApproval::Automatic)
        }))
    }

    /// Decisions are journaled before delivery. Never invent an option or allow
    /// an expired, cancelled or policy-invalid run to acquire a permission.
    pub fn automatic_approvals(&mut self, now: i64) -> Result<Vec<Effect>> {
        let pending: Vec<(String, String, String)> = {
            let mut stmt = self.store.db.prepare("SELECT a.id,a.run,a.options FROM approvals a JOIN runs r ON r.id=a.run WHERE a.consumed=0 AND a.expires>?1 AND r.status='waiting_approval'")?;
            stmt.query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut effects = vec![];
        for (request_id, run_id, raw) in pending {
            let run = self.store.run(&run_id)?.expect("queried run exists");
            let policy: String = self.store.db.query_row(
                "SELECT policy FROM conversations WHERE key=?1",
                [&run.conversation.key],
                |r| r.get(0),
            )?;
            if policy != self.config.binding_fingerprint(&run.conversation.room_id)
                || !self.automatic_tools(&run.conversation)?
            {
                continue;
            }
            let options: Vec<PermissionOption> = serde_json::from_str(&raw)?;
            let Some(option_id) = allow_option(&options) else {
                continue;
            };
            let tx = self.store.db.transaction()?;
            tx.execute("UPDATE approvals SET consumed=1 WHERE id=?1", [&request_id])?;
            tx.execute(
                "INSERT INTO approval_decisions VALUES(?1,?2,'automatic')",
                params![request_id, option_id],
            )?;
            tx.execute("UPDATE runs SET status='running' WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM approvals WHERE run=?1 AND consumed=0)", [&run_id])?;
            tx.commit()?;
            effects.push(Effect::Decide {
                run_id,
                request_id,
                option_id: Some(option_id),
            });
        }
        Ok(effects)
    }

    /// A message racing the end of a turn becomes its immediate continuation.
    /// It is never rejected merely because the prior worker closed its channel.
    pub fn resume_pending_steers(&mut self, now: i64) -> Result<Vec<Effect>> {
        let pending: Vec<(String, String)> = {
            let mut stmt = self.store.db.prepare("SELECT DISTINCT c.key,c.room FROM run_steers s JOIN runs r ON r.id=s.run JOIN conversations c ON c.key=r.conversation WHERE s.status='pending' AND r.status IN ('completed','cancelled')")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut effects = vec![];
        for (conversation, room) in pending {
            if self.store.active_run(&conversation)?.is_some() {
                continue;
            }
            let policy: String = self.store.db.query_row(
                "SELECT policy FROM conversations WHERE key=?1",
                [&conversation],
                |r| r.get(0),
            )?;
            if policy != self.config.binding_fingerprint(&room) {
                continue;
            }
            let rows: Vec<(String, String)> = {
                let mut stmt = self.store.db.prepare("SELECT s.event,s.prompt FROM run_steers s JOIN runs r ON r.id=s.run WHERE r.conversation=?1 AND s.status='pending' AND r.status IN ('completed','cancelled') ORDER BY s.rowid")?;
                stmt.query_map([&conversation], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?
            };
            if rows.is_empty() {
                continue;
            }
            let prompt = rows
                .iter()
                .map(|(_, p)| p.as_str())
                .collect::<Vec<_>>()
                .join("\n\nAdditional instruction:\n");
            let id = uuid::Uuid::new_v4().to_string();
            let tx = self.store.db.transaction()?;
            tx.execute(
                "INSERT INTO runs VALUES(?1,?2,?3,'queued',?4)",
                params![id, conversation, prompt, now],
            )?;
            tx.execute(
                "INSERT INTO run_inputs VALUES(?1,?2)",
                params![id, rows.last().expect("nonempty").0],
            )?;
            for (event, _) in rows {
                tx.execute(
                    "UPDATE run_steers SET run=?1,status='delivered' WHERE event=?2",
                    params![id, event],
                )?;
            }
            tx.commit()?;
            effects.push(Effect::Start { run_id: id });
        }
        Ok(effects)
    }

    pub fn flush_progress(&mut self, now: i64) -> Result<()> {
        let ids: Vec<String> = {
            let mut stmt = self.store.db.prepare("SELECT DISTINCT run FROM output")?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            if let Some(run) = self.store.run(&id)? {
                let tx = self.store.db.transaction()?;
                flush_run(&tx, &run, now)?;
                tx.commit()?;
            }
        }
        Ok(())
    }

    pub fn expire_approvals(&mut self, now: i64) -> Result<Vec<Effect>> {
        let tx = self.store.db.transaction()?;
        let expired: Vec<(String, String, String)> = {
            let mut stmt = tx.prepare("SELECT a.id,a.run,r.conversation FROM approvals a JOIN runs r ON r.id=a.run WHERE a.consumed=0 AND a.expires<=?1")?;
            stmt.query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut effects = vec![];
        for (request_id, run_id, key) in expired {
            tx.execute("UPDATE approvals SET consumed=1 WHERE id=?1", [&request_id])?;
            enqueue(
                &tx,
                &key,
                "Approval expired; the request was cancelled.",
                now,
            )?;
            effects.push(Effect::Decide {
                run_id: run_id.clone(),
                request_id,
                option_id: None,
            });
            tx.execute("UPDATE runs SET status='running' WHERE id=?1 AND status='waiting_approval' AND NOT EXISTS(SELECT 1 FROM approvals WHERE run=?1 AND consumed=0)",[run_id])?;
        }
        tx.commit()?;
        Ok(effects)
    }

    /// Re-check before delivery, not just when the original prompt was accepted.
    pub fn may_deliver(&self, message: &Outbound, snapshot: &RoomSnapshot) -> Result<bool> {
        let Some(policy) = self.config.room(&message.conversation.room_id) else {
            return Ok(false);
        };
        let expected: Option<(String, String)> = self
            .store
            .db
            .query_row(
                "SELECT policy,audience FROM conversations WHERE key=?1",
                [&message.conversation.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(snapshot.joined
            && snapshot.encrypted
            && snapshot.members.contains(&self.config.bot_user_id)
            && policy.permits_members(&snapshot.members)
            && expected.is_some_and(|(p, a)| {
                p == self
                    .config
                    .binding_fingerprint(&message.conversation.room_id)
                    && a == policy.audience_binding(&snapshot.members)
            }))
    }

    /// An audience change also stops active work, even if nobody sends a new prompt.
    pub fn reconcile_room(
        &mut self,
        room_id: &str,
        snapshot: &RoomSnapshot,
    ) -> Result<Vec<Effect>> {
        let active: Vec<String> = {
            let mut stmt = self.store.db.prepare("SELECT r.id FROM runs r JOIN conversations c ON c.key=r.conversation WHERE c.room=?1 AND (r.status IN ('queued','running','waiting_approval','cancelling') OR EXISTS(SELECT 1 FROM run_steers s WHERE s.run=r.id AND s.status='pending'))")?;
            stmt.query_map([room_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut effects = vec![];
        for id in active {
            let run = self.store.run(&id)?.expect("queried run exists");
            let probe = Outbound {
                reaction: None,
                transaction_id: String::new(),
                conversation: run.conversation,
                body: String::new(),
            };
            if !self.may_deliver(&probe, snapshot)? {
                self.store.db.execute(
                    "UPDATE run_steers SET status='cancelled' WHERE run=?1 AND status='pending'",
                    [&id],
                )?;
                if !run.status.active() {
                    continue;
                }
                self.store
                    .db
                    .execute("UPDATE runs SET status='cancelling' WHERE id=?1", [&id])?;
                self.store
                    .db
                    .execute("UPDATE approvals SET consumed=1 WHERE run=?1", [&id])?;
                effects.push(Effect::Cancel { run_id: id });
            }
        }
        Ok(effects)
    }
}

fn flush_run(db: &rusqlite::Connection, run: &Run, now: i64) -> Result<()> {
    let text: String = {
        let mut stmt = db.prepare("SELECT body FROM output WHERE run=?1 ORDER BY seq")?;
        stmt.query_map([&run.id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .concat()
    };
    if !text.is_empty() {
        enqueue(db, &run.conversation.key, &text, now)?;
    }
    db.execute("DELETE FROM output WHERE run=?1", [&run.id])?;
    Ok(())
}

fn allow_option(options: &[PermissionOption]) -> Option<String> {
    options
        .iter()
        .find(|o| o.kind == "allow_once")
        .or_else(|| options.iter().find(|o| o.kind == "allow_always"))
        .map(|o| o.id.clone())
}
