mod common;
use common::*;
use matrix_acp_bridge::{model::*, offline::Behavior};

#[tokio::test]
async fn active_followup_waits_for_running_tool_and_continues_same_session() {
    let (mut runner, agent) = fixture(Behavior::SteerThenReply);
    runner
        .ingest(&message("one", "original work", None), &room(), 100)
        .await
        .unwrap();
    // The fixture holds a tool until explicitly released by this test.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if agent.observations.lock().unwrap().prompts.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let handled = runner
        .ingest(
            &message("steer", "change the ongoing work", Some("$one")),
            &room(),
            101,
        )
        .await
        .unwrap();
    assert!(matches!(handled.effects.as_slice(), [Effect::Steer { .. }]));
    // Let the controller consume the follow-up while the tool remains held.
    tokio::task::yield_now().await;
    assert!(!agent.observations.lock().unwrap().tool_finished);
    assert_eq!(agent.observations.lock().unwrap().cancellations, 0);
    agent.release_tool.notify_one();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    assert!(agent.observations.lock().unwrap().tool_finished);
    assert_eq!(agent.observations.lock().unwrap().cancellations, 0);
    let obs = agent.observations.lock().unwrap();
    assert_eq!(obs.new_sessions, 1);
    assert_eq!(
        obs.prompts,
        vec!["original work", "change the ongoing work"]
    );
    assert_eq!(runner.bridge.store.count_runs().unwrap(), 1);
    assert!(runner.bridge.store.pending().unwrap().iter().any(|o| {
        o.reaction
            .as_ref()
            .is_some_and(|r| r.event_id == "$steer" && r.key == "✅")
    }));
}

#[test]
fn steer_racing_worker_completion_is_durable_and_becomes_one_continuation() {
    let mut bridge = bridge();
    let first = start(&mut bridge, "one");
    bridge
        .agent_event(
            AgentEvent::SessionReady {
                run_id: first.clone(),
                session_id: "existing-session".into(),
            },
            100,
        )
        .unwrap();
    let request = message("race", "new instruction", Some("$one"));
    let handled = bridge.handle(&request, &room(), 101).unwrap();
    assert!(matches!(handled.effects.as_slice(), [Effect::Steer { .. }]));
    // Force the exact interleaving: the worker closes before receiving Steer.
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: first,
                status: RunStatus::Completed,
            },
            102,
        )
        .unwrap();
    let effects = bridge.resume_pending_steers(103).unwrap();
    let [Effect::Start { run_id }] = effects.as_slice() else {
        panic!("expected continuation")
    };
    let run = bridge.started(run_id).unwrap();
    assert_eq!(run.prompt, "new instruction");
    assert_eq!(run.session_id.as_deref(), Some("existing-session"));
    assert!(bridge.resume_pending_steers(104).unwrap().is_empty());
    assert_eq!(
        bridge.handle(&request, &room(), 105).unwrap().disposition,
        Disposition::Duplicate
    );
}

#[test]
fn stopped_or_restarted_steering_is_not_replayed() {
    let mut bridge = bridge();
    let first = start(&mut bridge, "one");
    bridge
        .handle(&message("steer", "update", Some("$one")), &room(), 101)
        .unwrap();
    bridge
        .handle(&message("stop", "!bridge stop", Some("$one")), &room(), 102)
        .unwrap();
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: first,
                status: RunStatus::Cancelled,
            },
            103,
        )
        .unwrap();
    assert!(bridge.resume_pending_steers(104).unwrap().is_empty());
}

#[test]
fn unlimited_worker_mode_allows_independent_threads() {
    let mut bridge = bridge();
    bridge.config.max_concurrent_runs = 0;
    for index in 0..5 {
        start(&mut bridge, &format!("thread-{index}"));
    }
    assert_eq!(bridge.store.count_runs().unwrap(), 5);
}

#[test]
fn stop_after_worker_completion_cancels_racing_continuation() {
    let mut bridge = bridge();
    let first = start(&mut bridge, "one");
    bridge
        .handle(&message("steer", "update", Some("$one")), &room(), 101)
        .unwrap();
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: first,
                status: RunStatus::Completed,
            },
            102,
        )
        .unwrap();
    bridge
        .handle(&message("stop", "!bridge stop", Some("$one")), &room(), 103)
        .unwrap();
    assert!(bridge.resume_pending_steers(104).unwrap().is_empty());
}

#[tokio::test]
async fn newer_followup_does_not_overtake_a_pending_completion_race() {
    let (mut runner, _) = fixture(Behavior::SteerThenReply);
    let first = start(&mut runner.bridge, "one");
    runner
        .bridge
        .handle(
            &message("older", "older update", Some("$one")),
            &room(),
            101,
        )
        .unwrap();
    runner
        .bridge
        .agent_event(
            AgentEvent::Finished {
                run_id: first,
                status: RunStatus::Completed,
            },
            102,
        )
        .unwrap();
    let newer = runner
        .ingest(
            &message("newer", "newer update", Some("$one")),
            &room(),
            103,
        )
        .await
        .unwrap();
    assert!(matches!(newer.effects.as_slice(), [Effect::Steer { .. }]));
    assert_eq!(runner.bridge.store.count_runs().unwrap(), 2);
    runner.shutdown(104).await.unwrap();
}

#[tokio::test]
async fn native_grok_interjection_arrives_while_tool_is_still_running() {
    let (mut runner, agent) = fixture(Behavior::GrokInterject);
    runner.bridge.config.harness.steering = matrix_acp_bridge::config::Steering::GrokInterject;
    runner
        .ingest(&message("one", "original work", None), &room(), 100)
        .await
        .unwrap();
    loop {
        if matches!(next(&mut runner).await, AgentEvent::Tool { .. }) {
            break;
        }
    }
    runner
        .ingest(
            &message("steer", "new direction", Some("$one")),
            &room(),
            101,
        )
        .await
        .unwrap();
    loop {
        if matches!(next(&mut runner).await, AgentEvent::Steered { .. }) {
            break;
        }
    }
    {
        let obs = agent.observations.lock().unwrap();
        assert_eq!(obs.interjections, ["new direction"]);
        assert_eq!(obs.prompts, ["original work"]);
        assert!(!obs.tool_finished);
        assert_eq!(obs.cancellations, 0);
    }
    agent.release_tool.notify_one();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    assert!(agent.observations.lock().unwrap().tool_finished);
    assert_eq!(agent.observations.lock().unwrap().cancellations, 0);
}

#[tokio::test]
async fn concurrent_prompt_mode_does_not_close_original_turn_early() {
    let (mut runner, agent) = fixture(Behavior::CodexSteer);
    runner.bridge.config.harness.steering = matrix_acp_bridge::config::Steering::ConcurrentPrompt;
    runner
        .ingest(&message("one", "original work", None), &room(), 100)
        .await
        .unwrap();
    loop {
        if matches!(next(&mut runner).await, AgentEvent::Tool { .. }) {
            break;
        }
    }
    runner
        .ingest(
            &message("steer", "new direction", Some("$one")),
            &room(),
            101,
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while agent.observations.lock().unwrap().prompts.len() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!agent.observations.lock().unwrap().tool_finished);
    agent.release_tool.notify_one();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    assert!(agent.observations.lock().unwrap().tool_finished);
    assert_eq!(agent.observations.lock().unwrap().cancellations, 0);
}

#[tokio::test]
async fn grok_late_continuation_is_received_after_original_prompt_completes() {
    let (mut runner, agent) = fixture(Behavior::GrokLate);
    runner.bridge.config.harness.steering = matrix_acp_bridge::config::Steering::GrokInterject;
    runner
        .ingest(&message("one", "original work", None), &room(), 100)
        .await
        .unwrap();
    loop {
        if matches!(next(&mut runner).await, AgentEvent::Tool { .. }) {
            break;
        }
    }
    runner
        .ingest(
            &message("steer", "late direction", Some("$one")),
            &room(),
            101,
        )
        .await
        .unwrap();
    loop {
        if matches!(next(&mut runner).await, AgentEvent::Steered { .. }) {
            break;
        }
    }
    agent.release_tool.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while agent.observations.lock().unwrap().roster_checks == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // Primary ACP response is finished, but the provider-owned follow-up is held.
    assert!(agent.observations.lock().unwrap().tool_finished);
    agent
        .finish_native
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut saw_late = false;
    loop {
        match next(&mut runner).await {
            AgentEvent::Text { text, .. } if text == "Late follow-up response" => saw_late = true,
            AgentEvent::Finished { status, .. } => {
                assert!(saw_late);
                assert_eq!(status, RunStatus::Completed);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(agent.observations.lock().unwrap().cancellations, 0);
}

#[test]
fn native_input_ack_does_not_split_an_assistant_sentence() {
    let mut bridge = bridge();
    bridge.config.harness.steering = matrix_acp_bridge::config::Steering::GrokInterject;
    let run_id = start(&mut bridge, "one");
    bridge
        .agent_event(
            AgentEvent::Text {
                run_id: run_id.clone(),
                text: "Thanks, ".into(),
            },
            101,
        )
        .unwrap();
    bridge
        .handle(&message("follow", "new note", Some("$one")), &room(), 102)
        .unwrap();
    bridge
        .agent_event(
            AgentEvent::Steered {
                run_id: run_id.clone(),
                event_id: "$follow".into(),
            },
            103,
        )
        .unwrap();
    bridge
        .agent_event(
            AgentEvent::Text {
                run_id: run_id.clone(),
                text: "teammate!".into(),
            },
            104,
        )
        .unwrap();
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id,
                status: RunStatus::Completed,
            },
            105,
        )
        .unwrap();
    let messages = bridge.store.pending().unwrap();
    assert!(messages.iter().any(|m| m.body == "Thanks, teammate!"));
    assert!(!messages.iter().any(|m| m.body == "Thanks, "));
}
