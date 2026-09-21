mod common;
use common::*;
use matrix_acp_bridge::{model::*, offline::Behavior};

#[tokio::test]
async fn real_sdk_protocol_streams_resumes_and_suppresses_history_and_reasoning() {
    let (mut runner, agent) = fixture(Behavior::Reply);
    let first = runner
        .ingest(&message("one", "first request", None), &room(), 100)
        .await
        .unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    assert_eq!(agent.observations.lock().unwrap().new_sessions, 1);
    let Effect::Start { run_id } = &first.effects[0] else {
        panic!()
    };
    assert_eq!(
        runner
            .bridge
            .store
            .run(run_id)
            .unwrap()
            .unwrap()
            .session_id
            .as_deref(),
        Some("fixture-1")
    );
    runner
        .ingest(&message("two", "follow up", Some("$one")), &room(), 102)
        .await
        .unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    let obs = agent.observations.lock().unwrap();
    assert_eq!(obs.new_sessions, 1);
    assert_eq!(obs.loaded, vec!["fixture-1"]);
    assert_eq!(obs.prompts, vec!["first request", "follow up"]);
    assert!(obs.client_tools_disabled);
    let transcript = runner
        .bridge
        .store
        .pending()
        .unwrap()
        .into_iter()
        .map(|o| o.body)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(transcript.contains("Fixture response: first request"));
    assert!(transcript.contains("Fixture response: follow up"));
    assert!(!transcript.contains("OLD HISTORY"));
    assert!(!transcript.contains("PRIVATE FIXTURE"));
}

#[tokio::test]
async fn approval_round_trip_returns_exact_offered_option_to_agent() {
    let (mut runner, agent) = fixture(Behavior::Approval);
    runner
        .ingest(&message("one", "request permission", None), &room(), 100)
        .await
        .unwrap();
    let id = loop {
        if let AgentEvent::Permission { request_id, .. } = next(&mut runner).await {
            break request_id;
        }
    };
    assert!(agent.observations.lock().unwrap().decisions.is_empty());
    let handled = runner
        .ingest(
            &message(
                "approve",
                &format!("!bridge approve {id} permit-once"),
                Some("$one"),
            ),
            &room(),
            102,
        )
        .await
        .unwrap();
    assert_eq!(handled.effects.len(), 1);
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    assert!(agent.observations.lock().unwrap().decisions[0].contains("permit-once"));
}

#[tokio::test]
async fn expired_approval_returns_protocol_cancellation() {
    let (mut runner, agent) = fixture(Behavior::Approval);
    runner
        .ingest(&message("one", "request permission", None), &room(), 100)
        .await
        .unwrap();
    loop {
        if matches!(next(&mut runner).await, AgentEvent::Permission { .. }) {
            break;
        }
    }
    runner.tick(401).await.unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    assert!(agent.observations.lock().unwrap().decisions[0].contains("cancelled"));
}

#[tokio::test]
async fn cancellation_interrupts_an_in_flight_prompt() {
    let (mut runner, _) = fixture(Behavior::WaitForCancel);
    runner
        .ingest(&message("one", "wait", None), &room(), 100)
        .await
        .unwrap();
    assert!(matches!(
        next(&mut runner).await,
        AgentEvent::SessionReady { .. }
    ));
    runner
        .ingest(&message("stop", "!bridge stop", Some("$one")), &room(), 102)
        .await
        .unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Cancelled);
}

#[tokio::test]
async fn required_permission_mode_failure_prevents_any_prompt() {
    let (mut runner, agent) = fixture(Behavior::RejectMode);
    runner
        .ingest(&message("one", "must not run", None), &room(), 100)
        .await
        .unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Failed);
    assert!(agent.observations.lock().unwrap().prompts.is_empty());
}

#[tokio::test]
async fn session_mismatch_updates_are_not_posted() {
    let (mut runner, _) = fixture(Behavior::WrongSession);
    runner
        .ingest(&message("one", "must not leak", None), &room(), 100)
        .await
        .unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    assert!(
        !runner
            .bridge
            .store
            .pending()
            .unwrap()
            .iter()
            .any(|o| o.body.contains("Fixture response"))
    );
}

#[tokio::test]
async fn unsolicited_permission_mode_escalation_stops_the_connection() {
    let (mut runner, _) = fixture(Behavior::ChangeMode);
    runner
        .ingest(&message("one", "mode test", None), &room(), 100)
        .await
        .unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Failed);
}

#[tokio::test]
async fn replayed_matrix_event_does_not_launch_a_second_agent_turn() {
    let (mut runner, agent) = fixture(Behavior::Reply);
    let event = message("one", "once", None);
    runner.ingest(&event, &room(), 100).await.unwrap();
    finish(&mut runner).await;
    assert_eq!(
        runner
            .ingest(&event, &room(), 103)
            .await
            .unwrap()
            .disposition,
        Disposition::Duplicate
    );
    assert_eq!(agent.observations.lock().unwrap().prompts.len(), 1);
}

#[tokio::test]
async fn audience_change_cancels_running_work_and_holds_output() {
    let (mut runner, _) = fixture(Behavior::WaitForCancel);
    runner
        .ingest(&message("one", "wait", None), &room(), 100)
        .await
        .unwrap();
    next(&mut runner).await;
    let mut changed = room();
    changed.members.insert("@new:example.invalid".into());
    runner
        .reconcile_room(&config().rooms[0].room_id, &changed, 102)
        .await
        .unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Cancelled);
    for output in runner.bridge.store.pending().unwrap() {
        assert!(!runner.bridge.may_deliver(&output, &changed).unwrap());
    }
}

#[tokio::test]
async fn cancellation_while_waiting_for_human_does_not_deadlock_dispatch() {
    let (mut runner, agent) = fixture(Behavior::Approval);
    runner
        .ingest(&message("one", "ask", None), &room(), 100)
        .await
        .unwrap();
    loop {
        if matches!(next(&mut runner).await, AgentEvent::Permission { .. }) {
            break;
        }
    }
    runner
        .ingest(&message("stop", "!bridge stop", Some("$one")), &room(), 102)
        .await
        .unwrap();
    finish(&mut runner).await;
    assert!(agent.observations.lock().unwrap().decisions[0].contains("cancelled"));
}

#[tokio::test]
async fn doctor_checks_modes_without_prompting_or_loading_a_conversation() {
    let agent = matrix_acp_bridge::offline::FixtureAgent::new(Behavior::Reply);
    let report = matrix_acp_bridge::acp::inspect(agent.transport(), config().harness)
        .await
        .unwrap();
    assert!(report.can_resume && report.mode_applied);
    assert_eq!(report.modes, ["read-only"]);
    let obs = agent.observations.lock().unwrap();
    assert_eq!(obs.new_sessions, 1);
    assert!(obs.prompts.is_empty() && obs.loaded.is_empty());
    assert!(obs.client_tools_disabled);
}

#[tokio::test]
async fn doctor_reports_available_modes_and_does_not_fall_back() {
    let agent = matrix_acp_bridge::offline::FixtureAgent::new(Behavior::Reply);
    let mut harness = config().harness;
    harness.mode = Some("unavailable".into());
    let report = matrix_acp_bridge::acp::inspect(agent.transport(), harness)
        .await
        .unwrap();
    assert!(!report.mode_applied);
    assert_eq!(report.modes, ["read-only"]);
    assert!(agent.observations.lock().unwrap().prompts.is_empty());
}

#[tokio::test]
async fn modes_may_be_omitted_only_when_configuration_does_not_require_one() {
    let agent = matrix_acp_bridge::offline::FixtureAgent::new(Behavior::NoModes);
    let mut harness = config().harness;
    let rejected = matrix_acp_bridge::acp::inspect(agent.transport(), harness.clone())
        .await
        .unwrap();
    assert!(!rejected.mode_applied);
    harness.mode = None;
    let report = matrix_acp_bridge::acp::inspect(agent.transport(), harness)
        .await
        .unwrap();
    assert!(report.mode_applied && report.modes.is_empty());
}
