use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};

use crate::{
    config::{Config, ConversationMode, digest},
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

    fn authorized(&self, event: &Incoming, room: &RoomSnapshot) -> bool {
        let Some(policy) = self.config.room(&event.room_id) else {
            return false;
        };
        event.encrypted
            && event.verified_device
            && room.joined
            && room.encrypted
            && policy.operators.contains(&event.sender)
            && room.members.contains(&event.sender)
            && room.members.contains(&self.config.bot_user_id)
            && room.members.is_subset(&policy.audience)
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

    pub fn handle(&mut self, event: &Incoming, room: &RoomSnapshot, now: i64) -> Result<Handled> {
        if !self.authorized(event, room) {
            return Ok(Handled::new(Disposition::Denied));
        }
        let conversation = self.conversation(event);
        let policy_hash = self.config.fingerprint();
        let audience_hash = digest(&serde_json::to_vec(&room.members)?);
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
            return Ok(Handled::new(Disposition::Denied));
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
            return Ok(Handled::new(Disposition::Ignored));
        }
        // Commands cannot create a conversation or discover unrelated stored sessions.
        if command && binding.is_none() {
            return Ok(Handled::new(Disposition::Ignored));
        }

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
                    if let Some((run_id, _)) = active {
                        tx.execute("UPDATE runs SET status='cancelling' WHERE id=?1", [&run_id])?;
                        tx.execute("UPDATE approvals SET consumed=1 WHERE run=?1", [&run_id])?;
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
                        "Commands: !bridge status; !bridge stop; !bridge approve <request-id> <offered-option-id>.",
                        now,
                    )?;
                }
            }
        } else if active.is_some() {
            enqueue(
                &tx,
                &conversation.key,
                "This conversation already has an active run. Wait for it to finish, or use !bridge stop before sending another instruction.",
                now,
            )?;
        } else if !event.body.trim().is_empty() {
            let count: i64 = tx.query_row(
                "SELECT COUNT(*) FROM runs WHERE status IN ('queued','running','waiting_approval','cancelling')",
                [],
                |r| r.get(0),
            )?;
            if count >= self.config.max_concurrent_runs as i64 {
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
            tx.execute("INSERT INTO runs(id,conversation,prompt,status,created) VALUES(?1,?2,?3,'queued',?4)", params![id,conversation.key,event.body,now])?;
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
                    let choices = options
                        .iter()
                        .map(|o| format!("{}: {} ({})", o.id, o.label, o.kind))
                        .collect::<Vec<_>>()
                        .join("\n");
                    enqueue(
                        &tx,
                        &run.conversation.key,
                        &format!(
                            "Approval requested: {title}\n{choices}\nReply in this conversation: !bridge approve {request_id} <option-id>\nExpires in {} seconds.",
                            self.config.approval_ttl_seconds
                        ),
                        now,
                    )?;
                }
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
                }
            }
        }
        tx.commit()?;
        Ok(())
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
            && snapshot.members.is_subset(&policy.audience)
            && expected.is_some_and(|(p, a)| {
                p == self.config.fingerprint()
                    && a == digest(
                        &serde_json::to_vec(&snapshot.members).expect("members serialize"),
                    )
            }))
    }

    /// An audience change also stops active work, even if nobody sends a new prompt.
    pub fn reconcile_room(
        &mut self,
        room_id: &str,
        snapshot: &RoomSnapshot,
    ) -> Result<Vec<Effect>> {
        let active: Vec<String> = {
            let mut stmt = self.store.db.prepare("SELECT r.id FROM runs r JOIN conversations c ON c.key=r.conversation WHERE c.room=?1 AND r.status IN ('queued','running','waiting_approval','cancelling')")?;
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
