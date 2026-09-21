mod common;
use common::*;
use matrix_acp_bridge::{context, core::Bridge, model::*, offline::Behavior, store::Store};

fn start_context(bridge: &mut Bridge, request: &Incoming, history: &[Incoming]) -> Run {
    let handled = bridge
        .handle_with_context(request, &room(), Some(history), 100)
        .unwrap();
    let [Effect::Start { run_id }] = handled.effects.as_slice() else {
        panic!("expected run");
    };
    let run = bridge.started(run_id).unwrap();
    bridge
        .agent_event(
            AgentEvent::SessionReady {
                run_id: run.id.clone(),
                session_id: "persisted-session".into(),
            },
            101,
        )
        .unwrap();
    run
}
fn complete(bridge: &mut Bridge, run: &Run) {
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: run.id.clone(),
                status: RunStatus::Completed,
            },
            102,
        )
        .unwrap();
}

#[tokio::test]
async fn baseline_once_then_only_each_sender_line_in_the_same_session() {
    let (mut runner, agent) = fixture(Behavior::Reply);
    let parent = message("one", "INITIAL_CONTEXT_SENTINEL", None);
    runner
        .ingest_with_context(&parent, &room(), Some(&[]), 100)
        .await
        .unwrap();
    finish(&mut runner).await;
    let mut history = vec![parent];
    for i in 0..8 {
        let event = message(
            &format!("reply-{i}"),
            &format!("new message {i}"),
            Some("$one"),
        );
        runner
            .ingest_with_context(&event, &room(), Some(&history), 103 + i)
            .await
            .unwrap();
        finish(&mut runner).await;
        history.push(event);
    }
    let obs = agent.observations.lock().unwrap();
    assert_eq!(obs.new_sessions, 1);
    assert_eq!(obs.loaded.len(), 8);
    assert!(obs.prompts[0].starts_with(context::SESSION_INSTRUCTIONS));
    for i in 0..8 {
        assert_eq!(
            obs.prompts[i + 1],
            format!("@owner:example.invalid: new message {i}")
        );
    }
}

#[test]
fn unseen_reader_context_is_delivered_once_and_own_replies_are_not_echoed() {
    let mut bridge = bridge();
    bridge.config.rooms[0].audience_policy =
        matrix_acp_bridge::config::AudiencePolicy::RoomMembership;
    let first = message("one", "start", None);
    let run = start_context(&mut bridge, &first, &[]);
    bridge
        .agent_event(
            AgentEvent::Text {
                run_id: run.id.clone(),
                text: "ALREADY_IN_AGENT_SESSION".into(),
            },
            102,
        )
        .unwrap();
    complete(&mut bridge, &run);
    let out = bridge
        .store
        .pending()
        .unwrap()
        .into_iter()
        .find(|o| o.body == "ALREADY_IN_AGENT_SESSION")
        .unwrap();
    bridge
        .store
        .delivered(&out.transaction_id, "$bot-reply")
        .unwrap();
    let mut bot = message("bot-reply", "ALREADY_IN_AGENT_SESSION", Some("$one"));
    bot.sender = bridge.config.bot_user_id.clone();
    let mut reader = message("reader", "NEW_READER_CONTEXT", Some("$one"));
    reader.sender = "@reader:example.invalid".into();
    let next = message("two", "followup", Some("$one"));
    let run = start_context(
        &mut bridge,
        &next,
        &[first.clone(), bot.clone(), reader.clone()],
    );
    assert_eq!(
        run.prompt,
        "[context] @reader:example.invalid: NEW_READER_CONTEXT\n@owner:example.invalid: followup"
    );
    complete(&mut bridge, &run);
    let third = message("three", "another", Some("$one"));
    let run = start_context(&mut bridge, &third, &[first, bot, reader, next]);
    assert_eq!(run.prompt, "@owner:example.invalid: another");
}

#[test]
fn restart_preserves_seen_messages_and_explicit_session_reset_gets_a_new_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let cfg = config();
    let mut bridge =
        Bridge::new(cfg.clone(), Store::open(&state, &cfg.bot_user_id).unwrap()).unwrap();
    let first = message("one", "HISTORY", None);
    let run = start_context(&mut bridge, &first, &[]);
    complete(&mut bridge, &run);
    drop(bridge);
    let mut store = Store::open(&state, &cfg.bot_user_id).unwrap();
    store.recover(200).unwrap();
    let mut bridge = Bridge::new(cfg.clone(), store).unwrap();
    let second = message("two", "NEW", Some("$one"));
    let run = start_context(&mut bridge, &second, std::slice::from_ref(&first));
    assert_eq!(run.prompt, "@owner:example.invalid: NEW");
    complete(&mut bridge, &run);
    drop(bridge);
    let db = rusqlite::Connection::open(state.join("journal.sqlite")).unwrap();
    db.execute("UPDATE conversations SET session=NULL", [])
        .unwrap();
    drop(db);
    let mut bridge =
        Bridge::new(cfg.clone(), Store::open(&state, &cfg.bot_user_id).unwrap()).unwrap();
    let third = message("three", "fresh session", Some("$one"));
    let run = start_context(&mut bridge, &third, &[first, second]);
    assert!(run.prompt.starts_with(context::SESSION_INSTRUCTIONS));
    assert!(run.prompt.contains("HISTORY"));
    assert!(run.prompt.contains("NEW"));
}

#[test]
fn failed_undelivered_input_is_not_mistaken_for_seen_context() {
    let mut bridge = bridge();
    let first = message("one", "MUST_NOT_BE_LOST", None);
    let run = start_context(&mut bridge, &first, &[]);
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: run.id,
                status: RunStatus::Failed,
            },
            102,
        )
        .unwrap();
    let next = message("two", "try with this", Some("$one"));
    let run = start_context(&mut bridge, &next, std::slice::from_ref(&first));
    assert!(run.prompt.starts_with(context::SESSION_INSTRUCTIONS));
    assert!(run.prompt.contains("MUST_NOT_BE_LOST"));
}

#[test]
fn pending_steering_excludes_previously_staged_context_and_completion_race_keeps_delta() {
    let mut bridge = bridge();
    let first = message("one", "BASELINE", None);
    let run = start_context(&mut bridge, &first, &[]);
    let second = message("two", "FIRST_DELTA", Some("$one"));
    let handled = bridge
        .handle_with_context(&second, &room(), Some(std::slice::from_ref(&first)), 101)
        .unwrap();
    assert!(
        matches!(handled.effects.as_slice(),[Effect::Steer {prompt,..}] if prompt=="@owner:example.invalid: FIRST_DELTA")
    );
    let third = message("three", "SECOND_DELTA", Some("$one"));
    let handled = bridge
        .handle_with_context(&third, &room(), Some(&[first.clone(), second.clone()]), 102)
        .unwrap();
    assert!(
        matches!(handled.effects.as_slice(),[Effect::Steer {prompt,..}] if prompt=="@owner:example.invalid: SECOND_DELTA")
    );
    complete(&mut bridge, &run);
    let effects = bridge.resume_pending_steers(103).unwrap();
    let [Effect::Start { run_id }] = effects.as_slice() else {
        panic!()
    };
    let continued = bridge.started(run_id).unwrap();
    assert_eq!(
        continued.prompt,
        "@owner:example.invalid: FIRST_DELTA\n@owner:example.invalid: SECOND_DELTA"
    );
    complete(&mut bridge, &continued);
    let fourth = message("four", "LAST", Some("$one"));
    let prepared = bridge
        .prepare_context(&fourth, &[first, second, third])
        .unwrap();
    assert_eq!(prepared.prompt, "@owner:example.invalid: LAST");
}

#[test]
fn legacy_successful_json_prompts_seed_the_ledger_without_replaying_history() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let cfg = config();
    let mut bridge =
        Bridge::new(cfg.clone(), Store::open(&state, &cfg.bot_user_id).unwrap()).unwrap();
    let first = message("one", "OLD_HISTORY", None);
    let run = start_context(&mut bridge, &first, &[]);
    complete(&mut bridge, &run);
    drop(bridge);
    let legacy = format!(
        "You are replying in a Matrix conversation.\nOld instructions\n\n{}",
        serde_json::json!({"transport":"matrix","history":[{"event_id":"$parent"}],"authorized_request":{"event_id":"$one"}})
    );
    let db = rusqlite::Connection::open(state.join("journal.sqlite")).unwrap();
    db.execute("UPDATE runs SET prompt=?1", [legacy]).unwrap();
    db.execute_batch("DROP TABLE context_batches; DROP TABLE context_sessions; DELETE FROM meta WHERE key='context-ledger-v1';").unwrap();
    drop(db);
    let bridge = Bridge::new(cfg.clone(), Store::open(&state, &cfg.bot_user_id).unwrap()).unwrap();
    let next = message("two", "SHORT_REPLY", Some("$one"));
    let prepared = bridge.prepare_context(&next, &[first]).unwrap();
    assert!(!prepared.initial);
    assert_eq!(prepared.prompt, "@owner:example.invalid: SHORT_REPLY");
}
