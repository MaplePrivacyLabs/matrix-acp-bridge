//! Durable input bookkeeping, separate from the compact model-facing text.
use crate::context::Prepared;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeSet;

pub fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS context_batches(
        input TEXT PRIMARY KEY, conversation TEXT NOT NULL REFERENCES conversations(key),
        run TEXT NOT NULL REFERENCES runs(id), events TEXT NOT NULL, initial INTEGER NOT NULL,
        state TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS context_sessions(conversation TEXT PRIMARY KEY REFERENCES conversations(key), session TEXT);")?;
    let migrated: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='context-ledger-v1')",
        [],
        |r| r.get(0),
    )?;
    if migrated {
        return Ok(());
    }
    // Existing successful sessions already received the old full JSON snapshots.
    // Seed their event IDs without replaying those snapshots or changing sessions.
    let runs: Vec<(String, String, String, Option<String>)> = {
        let mut stmt=db.prepare("SELECT r.id,r.conversation,r.prompt,i.event FROM runs r JOIN conversations c ON c.key=r.conversation LEFT JOIN run_inputs i ON i.run=r.id WHERE r.status='completed' AND c.session IS NOT NULL")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (run, conversation, prompt, input) in runs {
        let events = legacy_events(&prompt);
        let initial = !events.is_empty();
        let mut events = events;
        if let Some(input) = &input {
            events.insert(input.clone());
        }
        if !events.is_empty() {
            db.execute(
                "INSERT OR IGNORE INTO context_batches VALUES(?1,?2,?3,?4,?5,'delivered')",
                params![
                    input.unwrap_or_else(|| format!("legacy:{run}")),
                    conversation,
                    run,
                    serde_json::to_string(&events)?,
                    initial
                ],
            )?;
        }
    }
    let steers: Vec<(String, String, String, String)> = {
        let mut stmt=db.prepare("SELECT s.event,r.conversation,r.id,s.prompt FROM run_steers s JOIN runs r ON r.id=s.run JOIN conversations c ON c.key=r.conversation WHERE s.status='delivered' AND r.status='completed' AND c.session IS NOT NULL")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (input, conversation, run, prompt) in steers {
        let mut events = legacy_events(&prompt);
        let initial = !events.is_empty();
        events.insert(input.clone());
        db.execute(
            "INSERT OR IGNORE INTO context_batches VALUES(?1,?2,?3,?4,?5,'delivered')",
            params![
                input,
                conversation,
                run,
                serde_json::to_string(&events)?,
                initial
            ],
        )?;
    }
    db.execute(
        "INSERT OR IGNORE INTO context_sessions SELECT key,session FROM conversations",
        [],
    )?;
    db.execute("INSERT INTO meta VALUES('context-ledger-v1','1')", [])?;
    Ok(())
}

fn legacy_events(prompt: &str) -> BTreeSet<String> {
    let mut events = BTreeSet::new();
    for part in prompt.split("\n\nAdditional instruction:\n") {
        if !part.starts_with("You are replying in a Matrix conversation.") {
            continue;
        }
        let Some((_, raw)) = part.split_once("\n\n") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        if value["transport"] != "matrix" {
            continue;
        }
        if let Some(history) = value["history"].as_array() {
            for message in history {
                if let Some(id) = message["event_id"].as_str() {
                    events.insert(id.into());
                }
            }
        }
        if let Some(id) = value["authorized_request"]["event_id"].as_str() {
            events.insert(id.into());
        }
    }
    events
}

pub fn known(db: &Connection, conversation: &str) -> Result<(BTreeSet<String>, bool)> {
    let current: Option<Option<String>> = db
        .query_row(
            "SELECT session FROM conversations WHERE key=?1",
            [conversation],
            |r| r.get(0),
        )
        .optional()?;
    let tracked: Option<Option<String>> = db
        .query_row(
            "SELECT session FROM context_sessions WHERE conversation=?1",
            [conversation],
            |r| r.get(0),
        )
        .optional()?;
    // An explicit session reset gets a fresh baseline, not a stale seen ledger.
    if tracked.is_some() && tracked != current {
        return Ok((BTreeSet::new(), true));
    }
    let mut seen = BTreeSet::new();
    let mut initialized = false;
    let mut stmt=db.prepare("SELECT b.events,b.initial FROM context_batches b JOIN runs r ON r.id=b.run WHERE b.conversation=?1 AND (b.state='delivered' OR (b.state IN ('pending','inflight') AND r.status IN ('queued','running','waiting_approval','cancelling')) OR (b.state='pending' AND r.status IN ('completed','cancelled') AND EXISTS(SELECT 1 FROM run_steers s WHERE s.event=b.input AND s.status='pending')))")?;
    for row in stmt.query_map([conversation], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
    })? {
        let (events, initial) = row?;
        seen.extend(serde_json::from_str::<Vec<String>>(&events)?);
        initialized |= initial;
    }
    if initialized {
        // Agent replies are already in this ACP session; do not echo Matrix copies.
        let mut stmt=db.prepare("SELECT delivered_event FROM outbox WHERE conversation=?1 AND delivered_event IS NOT NULL")?;
        seen.extend(
            stmt.query_map([conversation], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    Ok((seen, !initialized))
}

pub fn stage(
    db: &Connection,
    conversation: &str,
    run: &str,
    input: &str,
    prepared: &Prepared,
    steering: bool,
) -> Result<()> {
    let current: Option<String> = db.query_row(
        "SELECT session FROM conversations WHERE key=?1",
        [conversation],
        |r| r.get(0),
    )?;
    let tracked: Option<Option<String>> = db
        .query_row(
            "SELECT session FROM context_sessions WHERE conversation=?1",
            [conversation],
            |r| r.get(0),
        )
        .optional()?;
    if tracked.is_some_and(|tracked| tracked != current) {
        db.execute(
            "DELETE FROM context_batches WHERE conversation=?1",
            [conversation],
        )?;
    }
    db.execute(
        "INSERT OR REPLACE INTO context_sessions VALUES(?1,?2)",
        params![conversation, current],
    )?;
    db.execute(
        "INSERT INTO context_batches VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            input,
            conversation,
            run,
            serde_json::to_string(&prepared.events)?,
            prepared.initial,
            if steering { "pending" } else { "inflight" }
        ],
    )?;
    Ok(())
}

pub fn observed(db: &Connection, run: &str) -> Result<()> {
    db.execute(
        "UPDATE context_batches SET state='delivered' WHERE run=?1 AND state='inflight'",
        [run],
    )?;
    Ok(())
}
