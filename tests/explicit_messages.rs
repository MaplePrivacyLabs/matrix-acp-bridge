mod common;
use common::*;
use matrix_acp_bridge::{
    config::{AudiencePolicy, MessageDelivery},
    context::EXPLICIT_SEND_INSTRUCTIONS,
    messaging::SendHub,
    model::*,
    offline::Behavior,
};
use std::sync::Arc;

fn finish_event(bridge: &mut matrix_acp_bridge::core::Bridge, run: &str) {
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: run.into(),
                status: RunStatus::Completed,
            },
            110,
        )
        .unwrap();
}

#[test]
fn explicit_mode_never_publishes_assistant_output_even_on_completion_or_recovery() {
    let mut bridge = bridge();
    bridge.config.rooms[0].message_delivery = MessageDelivery::Explicit;
    let run = start(&mut bridge, "one");
    bridge
        .agent_event(
            AgentEvent::Text {
                run_id: run.clone(),
                text: "UNSENT_NARRATION".into(),
            },
            101,
        )
        .unwrap();
    bridge
        .agent_event(
            AgentEvent::Tool {
                run_id: run.clone(),
                title: "a tool".into(),
            },
            102,
        )
        .unwrap();
    finish_event(&mut bridge, &run);
    bridge.store.recover(120).unwrap();
    assert!(
        bridge
            .store
            .pending()
            .unwrap()
            .iter()
            .all(|m| m.reaction.is_some())
    );
    assert!(bridge.explicit_message(&run, "late send", 121).is_err());
}

#[tokio::test]
async fn scoped_send_waits_for_all_receipts_and_preserves_retry_ids() {
    let (mut runner, _) = fixture(Behavior::Reply);
    runner.bridge.config.rooms[0].message_delivery = MessageDelivery::Explicit;
    let (hub, rx) = SendHub::new();
    runner = runner.with_messaging(hub.clone(), rx, Arc::new(|_, _| vec![]));
    let run = start(&mut runner.bridge, "one");
    let _other = start(&mut runner.bridge, "two");
    let scope = hub.bind(&run);
    let token = scope.token.clone();
    let send = hub.send(&token, "A".repeat(6001));
    tokio::pin!(send);
    // Poll the real asynchronous request until it waits for a delivery receipt.
    assert!(futures::poll!(&mut send).is_pending());
    runner.poll_messages(101).unwrap();
    let outgoing: Vec<_> = runner
        .bridge
        .store
        .pending()
        .unwrap()
        .into_iter()
        .filter(|m| m.reaction.is_none())
        .collect();
    assert_eq!(outgoing.len(), 2);
    assert!(
        outgoing
            .iter()
            .all(|m| m.conversation.thread_root.as_deref() == Some("$one"))
    );
    let retry: Vec<_> = runner
        .bridge
        .store
        .pending()
        .unwrap()
        .into_iter()
        .filter(|m| m.reaction.is_none())
        .collect();
    assert_eq!(outgoing, retry);
    runner
        .bridge
        .store
        .delivered(&outgoing[0].transaction_id, "$sent-1")
        .unwrap();
    runner.poll_messages(102).unwrap();
    assert!(futures::poll!(&mut send).is_pending());
    runner
        .bridge
        .store
        .delivered(&outgoing[1].transaction_id, "$sent-2")
        .unwrap();
    runner.poll_messages(103).unwrap();
    let receipt = send.await.unwrap();
    assert!(receipt.sent);
    assert_eq!(receipt.event_ids, ["$sent-1", "$sent-2"]);
    assert_eq!(receipt.thread_root.as_deref(), Some("$one"));
    drop(scope);
    assert!(hub.send(&token, "stale connection".into()).await.is_err());
}

#[tokio::test]
async fn policy_block_returns_failure_and_does_not_send_later() {
    let (mut runner, _) = fixture(Behavior::Reply);
    runner.bridge.config.rooms[0].message_delivery = MessageDelivery::Explicit;
    let (hub, rx) = SendHub::new();
    runner = runner.with_messaging(hub.clone(), rx, Arc::new(|_, _| vec![]));
    let run = start(&mut runner.bridge, "one");
    let scope = hub.bind(&run);
    let send = hub.send(&scope.token, "explicit reply".into());
    tokio::pin!(send);
    assert!(futures::poll!(&mut send).is_pending());
    runner.poll_messages(101).unwrap();
    let outgoing = runner
        .bridge
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|m| m.reaction.is_none())
        .unwrap();
    let mut snapshot = room();
    snapshot.encrypted = false;
    assert!(!runner.bridge.may_deliver(&outgoing, &snapshot).unwrap());
    runner.block_message(&outgoing.transaction_id).unwrap();
    runner.poll_messages(102).unwrap();
    assert!(send.await.unwrap_err().contains("prevents delivery"));
    assert!(
        !runner
            .bridge
            .store
            .pending()
            .unwrap()
            .iter()
            .any(|m| m.transaction_id == outgoing.transaction_id)
    );
}

#[tokio::test]
async fn mode_change_instructs_existing_session_once_without_repeating_history() {
    let (mut runner, agent) = fixture(Behavior::Reply);
    runner.bridge.config.rooms[0].audience_policy = AudiencePolicy::RoomMembership;
    let first = message("one", "initial request", None);
    runner
        .ingest_with_context(&first, &room(), Some(&[]), 100)
        .await
        .unwrap();
    finish(&mut runner).await;
    runner.bridge.config.rooms[0].message_delivery = MessageDelivery::Explicit;
    let second = message("two", "next request", Some("$one"));
    runner
        .ingest_with_context(&second, &room(), Some(std::slice::from_ref(&first)), 102)
        .await
        .unwrap();
    finish(&mut runner).await;
    let third = message("three", "another request", Some("$one"));
    runner
        .ingest_with_context(&third, &room(), Some(&[first, second]), 104)
        .await
        .unwrap();
    finish(&mut runner).await;
    let obs = agent.observations.lock().unwrap();
    assert_eq!(obs.new_sessions, 1);
    assert!(obs.prompts[1].starts_with(EXPLICIT_SEND_INSTRUCTIONS));
    assert!(!obs.prompts[1].contains("initial request"));
    assert_eq!(obs.prompts[2], "@owner:example.invalid: another request");
}
