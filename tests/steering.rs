mod common;
use common::*;
use matrix_acp_bridge::{model::*, offline::Behavior};

#[tokio::test]
async fn active_followup_interrupts_and_resumes_same_acp_session_without_new_job() {
    let (mut runner, agent) = fixture(Behavior::SteerThenReply);
    runner
        .ingest(&message("one", "original work", None), &room(), 100)
        .await
        .unwrap();
    // The fixture records the first prompt before waiting for cancellation.
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
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
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
