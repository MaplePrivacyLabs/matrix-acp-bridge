mod common;
use common::*;
use matrix_acp_bridge::{core::Bridge, model::*, store::Store};

fn complete(bridge: &mut Bridge, run: &str, status: RunStatus) -> Outbound {
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: run.into(),
                status,
            },
            110,
        )
        .unwrap();
    bridge
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|m| m.reaction.as_ref().is_some_and(|r| r.key != "👀"))
        .unwrap()
}

#[test]
fn terminal_status_replaces_only_its_own_eyes_after_delivery() {
    for status in [
        RunStatus::Completed,
        RunStatus::Failed,
        RunStatus::Cancelled,
        RunStatus::Interrupted,
    ] {
        let mut bridge = bridge();
        let run = start(&mut bridge, "one");
        start(&mut bridge, "unrelated");
        let accepted = bridge.store.pending().unwrap();
        bridge
            .store
            .delivered(&accepted[0].transaction_id, "$eyes-one")
            .unwrap();
        bridge
            .store
            .delivered(&accepted[1].transaction_id, "$eyes-other")
            .unwrap();
        let terminal = complete(&mut bridge, &run, status);
        // A failed or still-pending terminal send must leave the eyes in place.
        assert!(bridge.store.pending_reaction_removals().unwrap().is_empty());
        bridge
            .store
            .delivered(&terminal.transaction_id, "$terminal")
            .unwrap();
        let cleanup = bridge.store.pending_reaction_removals().unwrap();
        assert_eq!(cleanup.len(), 1);
        assert_eq!(cleanup[0].event_id, "$eyes-one");
        assert_eq!(cleanup[0].conversation, terminal.conversation);
        assert!(
            bridge
                .may_deliver_to(&cleanup[0].conversation, &room())
                .unwrap()
        );
        let mut departed = room();
        departed.joined = false;
        assert!(
            !bridge
                .may_deliver_to(&cleanup[0].conversation, &departed)
                .unwrap()
        );
        // Repeated acknowledgements do not create additional cleanup sends.
        bridge
            .store
            .delivered(&terminal.transaction_id, "$terminal")
            .unwrap();
        assert_eq!(bridge.store.pending_reaction_removals().unwrap(), cleanup);
        bridge
            .store
            .reaction_removed(&cleanup[0].transaction_id, "$redaction")
            .unwrap();
        assert!(bridge.store.pending().unwrap().is_empty());
        assert!(bridge.store.pending_reaction_removals().unwrap().is_empty());
    }
}

#[test]
fn cleanup_targets_the_followup_message_without_removing_new_activity() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    let handled = bridge
        .handle(&message("two", "follow-up", Some("$one")), &room(), 101)
        .unwrap();
    assert!(matches!(handled.effects[0], Effect::Steer { .. }));
    bridge
        .agent_event(
            AgentEvent::Steered {
                run_id: run.clone(),
                event_id: "$two".into(),
            },
            102,
        )
        .unwrap();
    for (i, accepted) in bridge.store.pending().unwrap().iter().enumerate() {
        bridge
            .store
            .delivered(&accepted.transaction_id, &format!("$eyes-{i}"))
            .unwrap();
    }
    complete(&mut bridge, &run, RunStatus::Completed);
    for (i, terminal) in bridge.store.pending().unwrap().iter().enumerate() {
        bridge
            .store
            .delivered(&terminal.transaction_id, &format!("$done-{i}"))
            .unwrap();
    }
    let cleanup = bridge.store.pending_reaction_removals().unwrap();
    assert_eq!(
        cleanup
            .iter()
            .map(|m| m.event_id.as_str())
            .collect::<Vec<_>>(),
        ["$eyes-0", "$eyes-1"]
    );
    bridge
        .handle(&message("three", "new request", Some("$one")), &room(), 120)
        .unwrap();
    let newer = bridge
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|m| m.reaction.is_some())
        .unwrap();
    bridge
        .store
        .delivered(&newer.transaction_id, "$new-eyes")
        .unwrap();
    for redaction in cleanup {
        bridge
            .store
            .reaction_removed(&redaction.transaction_id, "$removed-old-eyes")
            .unwrap();
    }
    assert!(bridge.store.pending().unwrap().is_empty());
    assert!(bridge.store.pending_reaction_removals().unwrap().is_empty());
}

#[test]
fn cleanup_survives_restart_with_the_same_matrix_transaction_id() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let cfg = config();
    let mut bridge =
        Bridge::new(cfg.clone(), Store::open(&state, &cfg.bot_user_id).unwrap()).unwrap();
    let run = start(&mut bridge, "one");
    let eyes = bridge.store.pending().unwrap().remove(0);
    bridge
        .store
        .delivered(&eyes.transaction_id, "$eyes")
        .unwrap();
    let terminal = complete(&mut bridge, &run, RunStatus::Completed);
    bridge
        .store
        .delivered(&terminal.transaction_id, "$done")
        .unwrap();
    let cleanup = bridge.store.pending_reaction_removals().unwrap();
    assert_eq!(cleanup.len(), 1);
    drop(bridge);
    let mut store = Store::open(&state, &cfg.bot_user_id).unwrap();
    store.recover(120).unwrap();
    assert_eq!(store.pending_reaction_removals().unwrap(), cleanup);
    store
        .reaction_removed(&cleanup[0].transaction_id, "$removed")
        .unwrap();
    assert!(store.pending().unwrap().is_empty());
    assert!(store.pending_reaction_removals().unwrap().is_empty());
}
