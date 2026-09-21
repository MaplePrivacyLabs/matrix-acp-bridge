//! Session-scoped requests to the single-writer Matrix outbox.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Debug, Serialize)]
pub struct DeliveryReceipt {
    pub sent: bool,
    pub room_id: String,
    pub thread_root: Option<String>,
    pub event_ids: Vec<String>,
}

pub struct SendRequest {
    pub run_id: String,
    pub text: String,
    pub reply: oneshot::Sender<Result<DeliveryReceipt, String>>,
}

#[derive(Clone)]
pub struct SendHub {
    requests: mpsc::Sender<SendRequest>,
    scopes: Arc<Mutex<BTreeMap<String, String>>>,
}

pub struct ScopeLease {
    pub token: String,
    scopes: Arc<Mutex<BTreeMap<String, String>>>,
}
impl Drop for ScopeLease {
    fn drop(&mut self) {
        self.scopes
            .lock()
            .expect("send scopes lock")
            .remove(&self.token);
    }
}

impl SendHub {
    pub fn new() -> (Self, mpsc::Receiver<SendRequest>) {
        let (requests, receiver) = mpsc::channel(32);
        (
            Self {
                requests,
                scopes: Arc::default(),
            },
            receiver,
        )
    }
    pub fn bind(&self, run_id: &str) -> ScopeLease {
        let token = uuid::Uuid::new_v4().to_string();
        self.scopes
            .lock()
            .expect("send scopes lock")
            .insert(token.clone(), run_id.into());
        ScopeLease {
            token,
            scopes: self.scopes.clone(),
        }
    }
    pub fn valid(&self, token: &str) -> bool {
        self.scopes
            .lock()
            .expect("send scopes lock")
            .contains_key(token)
    }
    pub async fn send(&self, token: &str, text: String) -> Result<DeliveryReceipt, String> {
        let run_id = self
            .scopes
            .lock()
            .expect("send scopes lock")
            .get(token)
            .cloned()
            .ok_or("this tool connection has no active sending session")?;
        let (reply, result) = oneshot::channel();
        self.requests
            .send(SendRequest {
                run_id,
                text,
                reply,
            })
            .await
            .map_err(|_| "bridge stopped before accepting the send")?;
        result
            .await
            .map_err(|_| "delivery was not confirmed; the durable outbox may still send it")?
    }
}
