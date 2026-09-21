use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{Conversation, Outbound, Reaction, Run, RunStatus};

pub struct Store {
    pub(crate) db: Connection,
    _lock: Option<File>,
}

type RunRow = (
    String,
    String,
    Option<String>,
    String,
    String,
    Option<String>,
);

impl Store {
    pub fn memory(bot: &str) -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?, None, bot)
    }

    /// A private state directory and an exclusive worker lock prevent concurrent consumers.
    pub fn open(dir: &Path, bot: &str) -> Result<Self> {
        if dir.exists() {
            ensure!(
                !fs::symlink_metadata(dir)?.file_type().is_symlink(),
                "state directory cannot be a symlink"
            );
        } else {
            fs::create_dir_all(dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                fs::metadata(dir)?.permissions().mode() & 0o077 == 0,
                "state directory must be private (0700)"
            );
        }
        for name in [
            "worker.lock",
            "journal.sqlite",
            "journal.sqlite-wal",
            "journal.sqlite-shm",
        ] {
            if let Ok(meta) = fs::symlink_metadata(dir.join(name)) {
                ensure!(
                    meta.is_file() && !meta.file_type().is_symlink(),
                    "state files must be regular files"
                );
            }
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("worker.lock"))?;
        lock.try_lock_exclusive()
            .context("another worker owns this state directory")?;
        let db = Connection::open(dir.join("journal.sqlite"))?;
        Self::initialize(db, Some(lock), bot)
    }

    fn initialize(db: Connection, lock: Option<File>, bot: &str) -> Result<Self> {
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS inbox(room TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(room,event));
            CREATE TABLE IF NOT EXISTS conversations(key TEXT PRIMARY KEY, room TEXT NOT NULL, root TEXT, policy TEXT NOT NULL, audience TEXT NOT NULL, session TEXT);
            CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY, conversation TEXT NOT NULL REFERENCES conversations(key), prompt TEXT NOT NULL, status TEXT NOT NULL, created INTEGER NOT NULL);
            CREATE UNIQUE INDEX IF NOT EXISTS one_active_run ON runs(conversation) WHERE status IN ('queued','running','waiting_approval','cancelling');
            CREATE TABLE IF NOT EXISTS outbox(txn TEXT PRIMARY KEY, conversation TEXT NOT NULL REFERENCES conversations(key), body TEXT NOT NULL, delivered_event TEXT, created INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS approvals(id TEXT PRIMARY KEY, run TEXT NOT NULL REFERENCES runs(id), options TEXT NOT NULL, expires INTEGER NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS output(seq INTEGER PRIMARY KEY, run TEXT NOT NULL REFERENCES runs(id), body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS conversation_permissions(conversation TEXT PRIMARY KEY REFERENCES conversations(key), automatic INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS approval_decisions(request TEXT PRIMARY KEY REFERENCES approvals(id), option TEXT NOT NULL, source TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS run_inputs(run TEXT PRIMARY KEY REFERENCES runs(id), event TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS outbox_reactions(txn TEXT PRIMARY KEY REFERENCES outbox(txn), event TEXT NOT NULL, key TEXT NOT NULL);
        ")?;
        let version: Option<String> = db
            .query_row("SELECT value FROM meta WHERE key='schema'", [], |row| {
                row.get(0)
            })
            .optional()?;
        ensure!(
            version.as_deref().is_none_or(|v| v == "1" || v == "2"),
            "unsupported journal schema"
        );
        let owner: Option<String> = db
            .query_row("SELECT value FROM meta WHERE key='bot'", [], |row| {
                row.get(0)
            })
            .optional()?;
        ensure!(
            owner.as_deref().is_none_or(|value| value == bot),
            "journal belongs to another Matrix identity"
        );
        db.execute("INSERT OR REPLACE INTO meta VALUES('schema','2')", [])?;
        db.execute("INSERT OR IGNORE INTO meta VALUES('bot',?1)", [bot])?;
        Ok(Self { db, _lock: lock })
    }

    pub fn run(&self, id: &str) -> Result<Option<Run>> {
        let row: Option<RunRow> = self.db.query_row(
            "SELECT c.key,c.room,c.root,r.prompt,r.status,c.session FROM runs r JOIN conversations c ON c.key=r.conversation WHERE r.id=?1", [id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))
        ).optional()?;
        row.map(|(key, room_id, thread_root, prompt, status, session_id)| {
            Ok(Run {
                id: id.into(),
                conversation: Conversation {
                    key,
                    room_id,
                    thread_root,
                },
                prompt,
                status: serde_json::from_value(status.into())?,
                session_id,
            })
        })
        .transpose()
    }

    pub fn active_run(&self, key: &str) -> Result<Option<Run>> {
        let id: Option<String> = self.db.query_row("SELECT id FROM runs WHERE conversation=?1 AND status IN ('queued','running','waiting_approval','cancelling')", [key], |r|r.get(0)).optional()?;
        id.map(|id| self.run(&id)).transpose().map(Option::flatten)
    }

    pub fn pending(&self) -> Result<Vec<Outbound>> {
        let mut stmt = self.db.prepare("SELECT o.txn,c.key,c.room,c.root,o.body,x.event,x.key FROM outbox o JOIN conversations c ON c.key=o.conversation LEFT JOIN outbox_reactions x ON x.txn=o.txn WHERE o.delivered_event IS NULL ORDER BY o.rowid")?;
        Ok(stmt
            .query_map([], |r| {
                Ok(Outbound {
                    transaction_id: r.get(0)?,
                    conversation: Conversation {
                        key: r.get(1)?,
                        room_id: r.get(2)?,
                        thread_root: r.get(3)?,
                    },
                    body: r.get(4)?,
                    reaction: r
                        .get::<_, Option<String>>(5)?
                        .zip(r.get::<_, Option<String>>(6)?)
                        .map(|(event_id, key)| Reaction { event_id, key }),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delivered(&mut self, txn: &str, event: &str) -> Result<()> {
        self.db.execute(
            "UPDATE outbox SET delivered_event=?2 WHERE txn=?1 AND delivered_event IS NULL",
            params![txn, event],
        )?;
        Ok(())
    }

    pub fn count_runs(&self) -> Result<i64> {
        Ok(self
            .db
            .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))?)
    }

    pub fn sync_token(&self) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT value FROM meta WHERE key='sync_token'", [], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn checkpoint_sync(&mut self, token: &str) -> Result<()> {
        self.checkpoint_sync_with_anchors(token, &BTreeMap::new())
    }

    /// Commit the sync cursor and each room's latest seen event as one unit.
    pub fn checkpoint_sync_with_anchors(
        &mut self,
        token: &str,
        anchors: &BTreeMap<String, String>,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        let staged: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key='staged_token'", [], |r| {
                r.get(0)
            })
            .optional()?;
        ensure!(
            staged.as_deref().is_none_or(|s| s == token),
            "checkpoint does not match staged batch"
        );
        tx.execute("INSERT INTO meta(key,value) VALUES('sync_token',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[token])?;
        for (room, event) in anchors {
            tx.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![format!("room_anchor:{room}"), event])?;
        }
        tx.execute(
            "DELETE FROM meta WHERE key IN ('staged_token','staged_batch')",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn room_anchor(&self, room: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                [format!("room_anchor:{room}")],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Stage encrypted wire events before SDK processing can advance its own cursor.
    pub fn stage_sync(&mut self, token: &str, payload: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key='staged_token')",
            [],
            |r| r.get(0),
        )?;
        ensure!(!exists, "an unprocessed sync batch already exists");
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('staged_token',?1)",
            [token],
        )?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('staged_batch',?1)",
            [payload],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn staged_sync(&self) -> Result<Option<(String, String)>> {
        Ok(self.db.query_row("SELECT a.value,b.value FROM meta a JOIN meta b ON b.key='staged_batch' WHERE a.key='staged_token'",[],|r|Ok((r.get(0)?,r.get(1)?))).optional()?)
    }

    /// Restart must never replay ambiguous agent side effects automatically.
    pub fn recover(&mut self, now: i64) -> Result<usize> {
        let tx = self.db.transaction()?;
        let active: Vec<(String, String)> = {
            let mut stmt = tx.prepare("SELECT id,conversation FROM runs WHERE status IN ('queued','running','waiting_approval','cancelling')")?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        for (id, key) in &active {
            tx.execute("UPDATE runs SET status='interrupted' WHERE id=?1", [id])?;
            enqueue(
                &tx,
                key,
                &format!(
                    "Run {id} was interrupted. Its external effects may already have happened. Review the workspace before sending a new instruction."
                ),
                now,
            )?;
        }
        tx.execute("UPDATE approvals SET consumed=1 WHERE consumed=0", [])?;
        tx.commit()?;
        Ok(active.len())
    }

    pub(crate) fn set_status(&mut self, id: &str, status: RunStatus) -> Result<()> {
        self.db.execute(
            "UPDATE runs SET status=?2 WHERE id=?1",
            params![id, status.as_str()],
        )?;
        Ok(())
    }
}

pub(crate) fn enqueue(db: &Connection, key: &str, body: &str, now: i64) -> Result<()> {
    // Split by Unicode scalar boundaries. This limits Matrix event size, not agent output.
    let chars: Vec<char> = body.chars().collect();
    for chunk in chars.chunks(6000) {
        let text: String = chunk.iter().collect();
        db.execute(
            "INSERT INTO outbox(txn,conversation,body,created) VALUES(?1,?2,?3,?4)",
            params![uuid::Uuid::new_v4().to_string(), key, text, now],
        )?;
    }
    Ok(())
}

pub(crate) fn enqueue_reaction(
    db: &Connection,
    conversation: &str,
    event: &str,
    key: &str,
    now: i64,
) -> Result<()> {
    let txn = uuid::Uuid::new_v4().to_string();
    db.execute(
        "INSERT INTO outbox(txn,conversation,body,created) VALUES(?1,?2,'',?3)",
        params![txn, conversation, now],
    )?;
    db.execute(
        "INSERT INTO outbox_reactions(txn,event,key) VALUES(?1,?2,?3)",
        params![txn, event, key],
    )?;
    Ok(())
}
