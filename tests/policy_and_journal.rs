mod common;
use common::*;
use matrix_acp_bridge::{
    config::{Config, ConversationMode},
    core::Bridge,
    model::*,
    store::Store,
};

#[test]
fn closed_authorization_checks_every_axis() {
    for axis in 0..8 {
        let mut bridge = bridge();
        let mut event = message("first", "work", None);
        let mut snapshot = room();
        match axis {
            0 => event.sender = "@stranger:example.invalid".into(),
            1 => event.room_id = "!other:example.invalid".into(),
            2 => event.encrypted = false,
            3 => event.verified_device = false,
            4 => snapshot.encrypted = false,
            5 => snapshot.joined = false,
            6 => {
                snapshot.members.insert("@observer:example.invalid".into());
            }
            _ => {
                snapshot.members.remove(&event.sender);
            }
        }
        assert_eq!(
            bridge.handle(&event, &snapshot, 100).unwrap().disposition,
            Disposition::Denied,
            "axis {axis}"
        );
        assert_eq!(bridge.store.count_runs().unwrap(), 0);
        assert!(bridge.store.pending().unwrap().is_empty());
    }
}

#[test]
fn reconsidered_event_still_requires_current_authorization_and_runs_only_once() {
    let mut bridge = bridge();
    let mut event = message("previously-unverified", "harmless test", None);
    event.verified_device = false;
    assert_eq!(
        bridge.handle(&event, &room(), 100).unwrap().disposition,
        Disposition::Denied
    );
    event.verified_device = true;
    let mut changed = room();
    changed.members.insert("@unexpected:example.invalid".into());
    assert_eq!(
        bridge.handle(&event, &changed, 101).unwrap().disposition,
        Disposition::Denied
    );
    assert_eq!(bridge.store.count_runs().unwrap(), 0);
    assert_eq!(
        bridge.handle(&event, &room(), 102).unwrap().effects.len(),
        1
    );
    assert_eq!(
        bridge.handle(&event, &room(), 103).unwrap().disposition,
        Disposition::Duplicate
    );
    assert_eq!(bridge.store.count_runs().unwrap(), 1);
}

#[test]
fn mentions_trigger_but_room_size_does_not() {
    let mut bridge = bridge();
    let mut event = message("first", "normal chat", None);
    event.mentions.clear();
    assert_eq!(
        bridge.handle(&event, &room(), 100).unwrap().disposition,
        Disposition::Ignored
    );
    event.mentions.insert(config().bot_user_id);
    assert_eq!(
        bridge.handle(&event, &room(), 100).unwrap().effects.len(),
        1
    );
    assert_eq!(
        bridge.handle(&event, &room(), 100).unwrap().disposition,
        Disposition::Duplicate
    );
    assert_eq!(bridge.store.count_runs().unwrap(), 1);
}

#[test]
fn conversations_isolate_threads_and_serialize_work() {
    let mut bridge = bridge();
    let first = start(&mut bridge, "one");
    let mut followup = message("two", "follow-up", Some("$one"));
    followup.mentions.clear();
    assert!(
        bridge
            .handle(&followup, &room(), 101)
            .unwrap()
            .effects
            .is_empty()
    );
    let other = start(&mut bridge, "other");
    assert_ne!(
        bridge.store.run(&first).unwrap().unwrap().conversation.key,
        bridge.store.run(&other).unwrap().unwrap().conversation.key
    );
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: first,
                status: RunStatus::Completed,
            },
            102,
        )
        .unwrap();
    followup.event_id = "$three".into();
    assert_eq!(
        bridge
            .handle(&followup, &room(), 103)
            .unwrap()
            .effects
            .len(),
        1
    );
}

#[test]
fn room_mode_requires_mention_or_reply_even_after_binding() {
    let mut bridge = bridge();
    bridge.config.rooms[0].conversation = ConversationMode::Room;
    let run = start(&mut bridge, "one");
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: run,
                status: RunStatus::Completed,
            },
            101,
        )
        .unwrap();
    let mut ordinary = message("two", "ordinary chat", None);
    ordinary.mentions.clear();
    assert_eq!(
        bridge.handle(&ordinary, &room(), 102).unwrap().disposition,
        Disposition::Ignored
    );
    let sent = bridge.store.pending().unwrap()[0].clone();
    bridge
        .store
        .delivered(&sent.transaction_id, "$reply")
        .unwrap();
    ordinary.reply_to = Some("$reply".into());
    assert_eq!(
        bridge
            .handle(&ordinary, &room(), 102)
            .unwrap()
            .effects
            .len(),
        1
    );
}

#[test]
fn changed_authority_or_audience_cannot_reuse_context_or_deliver() {
    let mut bridge = bridge();
    start(&mut bridge, "one");
    let output = bridge.store.pending().unwrap()[0].clone();
    assert!(bridge.may_deliver(&output, &room()).unwrap());
    let mut changed = room();
    changed.members.insert("@new:example.invalid".into());
    bridge.config.rooms[0]
        .audience
        .insert("@new:example.invalid".into());
    assert!(!bridge.may_deliver(&output, &changed).unwrap());
    assert_eq!(
        bridge
            .handle(&message("two", "follow-up", Some("$one")), &changed, 102)
            .unwrap()
            .disposition,
        Disposition::Denied
    );
}

fn permission(bridge: &mut Bridge, run: &str) {
    bridge
        .agent_event(
            AgentEvent::Permission {
                run_id: run.into(),
                request_id: "approval-1".into(),
                title: "Fixture action".into(),
                options: vec![PermissionOption {
                    id: "yes-once".into(),
                    label: "Yes".into(),
                    kind: "allow_once".into(),
                }],
            },
            100,
        )
        .unwrap();
}

#[test]
fn approval_is_scoped_one_use_expiring_and_uses_offered_options() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    permission(&mut bridge, &run);
    start(&mut bridge, "other");
    let cross = message(
        "cross",
        "!bridge approve approval-1 yes-once",
        Some("$other"),
    );
    assert!(
        bridge
            .handle(&cross, &room(), 101)
            .unwrap()
            .effects
            .is_empty()
    );
    let invalid = message("bad", "!bridge approve approval-1 invented", Some("$one"));
    assert!(
        bridge
            .handle(&invalid, &room(), 102)
            .unwrap()
            .effects
            .is_empty()
    );
    let mut stranger = message(
        "stranger",
        "!bridge approve approval-1 yes-once",
        Some("$one"),
    );
    stranger.sender = "@stranger:example.invalid".into();
    assert_eq!(
        bridge.handle(&stranger, &room(), 103).unwrap().disposition,
        Disposition::Denied
    );
    let accepted = message(
        "accept",
        "!bridge approve approval-1 yes-once",
        Some("$one"),
    );
    assert_eq!(
        bridge.handle(&accepted, &room(), 104).unwrap().effects,
        vec![Effect::Decide {
            run_id: run,
            request_id: "approval-1".into(),
            option_id: Some("yes-once".into())
        }]
    );
    assert!(
        bridge
            .handle(
                &message("again", &accepted.body, Some("$one")),
                &room(),
                105
            )
            .unwrap()
            .effects
            .is_empty()
    );
}

#[test]
fn approval_expiry_cancels_and_cannot_be_extended_by_duplicates() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    permission(&mut bridge, &run);
    assert!(bridge.expire_approvals(399).unwrap().is_empty());
    assert_eq!(
        bridge.expire_approvals(400).unwrap(),
        vec![Effect::Decide {
            run_id: run,
            request_id: "approval-1".into(),
            option_id: None
        }]
    );
    assert!(bridge.expire_approvals(401).unwrap().is_empty());
    assert!(
        bridge
            .handle(
                &message("late", "!bridge approve approval-1 yes-once", Some("$one")),
                &room(),
                401
            )
            .unwrap()
            .effects
            .is_empty()
    );
}

#[test]
fn stopping_consumes_pending_approvals_and_prevents_overlapping_turn() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    permission(&mut bridge, &run);
    assert_eq!(
        bridge
            .handle(&message("stop", "!bridge stop", Some("$one")), &room(), 101)
            .unwrap()
            .effects,
        vec![Effect::Cancel { run_id: run }]
    );
    assert!(
        bridge
            .handle(
                &message("late", "!bridge approve approval-1 yes-once", Some("$one")),
                &room(),
                102
            )
            .unwrap()
            .effects
            .is_empty()
    );
    assert!(
        bridge
            .handle(&message("new", "New work", Some("$one")), &room(), 102)
            .unwrap()
            .effects
            .is_empty()
    );
}

#[test]
fn disk_restart_preserves_delivery_ids_and_never_replays_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let cfg = config();
    let mut bridge =
        Bridge::new(cfg.clone(), Store::open(&path, &cfg.bot_user_id).unwrap()).unwrap();
    let run = start(&mut bridge, "one");
    permission(&mut bridge, &run);
    let before = bridge.store.pending().unwrap();
    assert!(
        Store::open(&path, &cfg.bot_user_id).is_err(),
        "exclusive state ownership"
    );
    drop(bridge);
    let mut store = Store::open(&path, &cfg.bot_user_id).unwrap();
    assert_eq!(store.pending().unwrap(), before);
    assert_eq!(store.recover(500).unwrap(), 1);
    assert_eq!(store.recover(501).unwrap(), 0);
    assert_eq!(
        store.run(&run).unwrap().unwrap().status,
        RunStatus::Interrupted
    );
    let mut bridge = Bridge::new(cfg, store).unwrap();
    assert_eq!(
        bridge
            .handle(&message("one", "Do fixture work", None), &room(), 502)
            .unwrap()
            .disposition,
        Disposition::Duplicate
    );
    assert!(
        bridge
            .handle(
                &message("late", "!bridge approve approval-1 yes-once", Some("$one")),
                &room(),
                502
            )
            .unwrap()
            .effects
            .is_empty()
    );
    assert_eq!(bridge.store.count_runs().unwrap(), 1);
}

#[test]
fn journals_cannot_be_shared_by_distinct_bot_identities() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    drop(Store::open(&path, &config().bot_user_id).unwrap());
    assert!(Store::open(&path, "@other:example.invalid").is_err());
}

#[test]
fn encrypted_wire_batch_survives_restart_and_checkpoint_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state");
    let mut store = Store::open(&path, &config().bot_user_id).unwrap();
    store.checkpoint_sync("before").unwrap();
    store
        .stage_sync("after", r#"{"ciphertext":"offline-fixture"}"#)
        .unwrap();
    assert!(store.stage_sync("newer", "{}").is_err());
    assert!(store.checkpoint_sync("wrong").is_err());
    let anchors = std::collections::BTreeMap::from([(
        "!engineering:example.invalid".to_owned(),
        "$last".to_owned(),
    )]);
    assert!(
        store
            .checkpoint_sync_with_anchors("wrong", &anchors)
            .is_err()
    );
    assert_eq!(
        store.room_anchor("!engineering:example.invalid").unwrap(),
        None
    );
    drop(store);
    let mut store = Store::open(&path, &config().bot_user_id).unwrap();
    assert_eq!(store.sync_token().unwrap().as_deref(), Some("before"));
    assert_eq!(
        store.staged_sync().unwrap(),
        Some(("after".into(), r#"{"ciphertext":"offline-fixture"}"#.into()))
    );
    store
        .checkpoint_sync_with_anchors("after", &anchors)
        .unwrap();
    assert_eq!(store.sync_token().unwrap().as_deref(), Some("after"));
    assert_eq!(
        store
            .room_anchor("!engineering:example.invalid")
            .unwrap()
            .as_deref(),
        Some("$last")
    );
    assert!(store.staged_sync().unwrap().is_none());
}

#[test]
fn unicode_output_is_split_without_loss_and_completion_is_idempotent() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    for out in bridge.store.pending().unwrap() {
        bridge
            .store
            .delivered(&out.transaction_id, "$accepted")
            .unwrap();
    }
    let text = "🦀 café 日本語".repeat(3000);
    bridge
        .agent_event(
            AgentEvent::Text {
                run_id: run.clone(),
                text: text.clone(),
            },
            101,
        )
        .unwrap();
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: run.clone(),
                status: RunStatus::Completed,
            },
            102,
        )
        .unwrap();
    let out = bridge.store.pending().unwrap();
    assert_eq!(
        out[..out.len() - 1]
            .iter()
            .map(|o| o.body.as_str())
            .collect::<String>(),
        text
    );
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: run,
                status: RunStatus::Failed,
            },
            103,
        )
        .unwrap();
    assert_eq!(bridge.store.pending().unwrap(), out);
}

#[test]
fn configuration_typos_and_invalid_policy_fail_closed() {
    let source = include_str!("../config/example.toml");
    assert!(Config::parse(&source.replace("operators =", "operatorz =")).is_err());
    assert!(Config::parse(&source.replace("https://matrix", "http://matrix")).is_err());
    assert!(Config::parse(&source.replace("mode = \"read-only\"", "mode = \"\"")).is_err());
    let mut cfg = config();
    cfg.rooms[0].audience.clear();
    assert!(cfg.validate().is_err());
}

#[test]
fn configured_concurrency_prevents_unbounded_worker_launches() {
    let mut bridge = bridge();
    bridge.config.max_concurrent_runs = 1;
    start(&mut bridge, "one");
    assert!(
        bridge
            .handle(&message("two", "more work", None), &room(), 101)
            .unwrap()
            .effects
            .is_empty()
    );
    assert_eq!(bridge.store.count_runs().unwrap(), 1);
}

#[test]
fn outbox_preserves_causal_order_even_if_wall_clock_moves_backwards() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    bridge
        .agent_event(
            AgentEvent::Text {
                run_id: run.clone(),
                text: "result".into(),
            },
            90,
        )
        .unwrap();
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: run,
                status: RunStatus::Completed,
            },
            91,
        )
        .unwrap();
    let out = bridge.store.pending().unwrap();
    assert_eq!(
        out[0].reaction.as_ref().unwrap(),
        &Reaction {
            event_id: "$one".into(),
            key: "👀".into()
        }
    );
    assert_eq!(out[1].body, "result");
    assert_eq!(
        out[2].reaction.as_ref().unwrap(),
        &Reaction {
            event_id: "$one".into(),
            key: "✅".into()
        }
    );
}

#[test]
fn late_permission_after_stop_cannot_reopen_approval() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    bridge
        .handle(&message("stop", "!bridge stop", Some("$one")), &room(), 101)
        .unwrap();
    permission(&mut bridge, &run);
    assert_eq!(
        bridge.store.run(&run).unwrap().unwrap().status,
        RunStatus::Cancelling
    );
    assert!(
        bridge
            .handle(
                &message(
                    "approve",
                    "!bridge approve approval-1 yes-once",
                    Some("$one")
                ),
                &room(),
                102
            )
            .unwrap()
            .effects
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn symlinked_or_public_state_is_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Store::open(&target, &config().bot_user_id).is_err());
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.path().join("link");
    symlink(&target, &link).unwrap();
    assert!(Store::open(&link, &config().bot_user_id).is_err());
    symlink(dir.path().join("outside"), target.join("journal.sqlite")).unwrap();
    assert!(Store::open(&target, &config().bot_user_id).is_err());
}

#[test]
fn followup_status_reactions_target_the_followup_message() {
    let mut bridge = bridge();
    let first = start(&mut bridge, "one");
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: first,
                status: RunStatus::Completed,
            },
            101,
        )
        .unwrap();
    let handled = bridge
        .handle(&message("two", "next task", Some("$one")), &room(), 102)
        .unwrap();
    let Effect::Start { run_id } = &handled.effects[0] else {
        panic!("expected followup run")
    };
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: run_id.clone(),
                status: RunStatus::Failed,
            },
            103,
        )
        .unwrap();
    let reactions: Vec<_> = bridge
        .store
        .pending()
        .unwrap()
        .into_iter()
        .filter_map(|item| item.reaction)
        .collect();
    assert_eq!(
        reactions
            .iter()
            .map(|r| (r.event_id.as_str(), r.key.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("$one", "👀"),
            ("$one", "✅"),
            ("$two", "👀"),
            ("$two", "❌")
        ]
    );
}

#[test]
fn schema_one_upgrade_preserves_existing_runs_and_text_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config();
    let state = dir.path().join("private-state");
    let mut bridge =
        Bridge::new(cfg.clone(), Store::open(&state, &cfg.bot_user_id).unwrap()).unwrap();
    start(&mut bridge, "one");
    let original = bridge.store.pending().unwrap()[0].clone();
    drop(bridge);
    let db = rusqlite::Connection::open(state.join("journal.sqlite")).unwrap();
    db.execute_batch("DROP TABLE outbox_reactions; DROP TABLE run_inputs; UPDATE meta SET value='1' WHERE key='schema'; UPDATE outbox SET body='Legacy accepted status';").unwrap();
    drop(db);
    let store = Store::open(&state, &cfg.bot_user_id).unwrap();
    assert_eq!(store.count_runs().unwrap(), 1);
    let preserved = &store.pending().unwrap()[0];
    assert_eq!(preserved.transaction_id, original.transaction_id);
    assert_eq!(preserved.body, "Legacy accepted status");
    assert!(preserved.reaction.is_none());
}
