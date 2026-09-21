mod common;
use common::*;
use matrix_acp_bridge::{config::ToolApproval, context, model::*, offline::Behavior};

#[tokio::test]
async fn reader_parent_and_complete_thread_reach_acp_without_granting_reader_authority() {
    let (mut runner, agent) = fixture(Behavior::Reply);
    let reader = "@reader:example.invalid";
    runner.bridge.config.rooms[0].audience.insert(reader.into());
    let mut snapshot = room();
    snapshot.members.insert(reader.into());
    let mut parent = message(
        "reader-parent",
        "How many r characters are in strawberry?",
        None,
    );
    parent.sender = reader.into();
    parent.verified_device = false;
    assert_eq!(
        runner
            .ingest(&parent, &snapshot, 100)
            .await
            .unwrap()
            .disposition,
        Disposition::Denied
    );
    assert!(agent.observations.lock().unwrap().prompts.is_empty());

    // More than one Matrix page of prior messages: the prompt must not truncate.
    let mut history = vec![parent.clone()];
    for i in 0..205 {
        let mut entry = message(
            &format!("context-{i}"),
            &format!("detail {i}"),
            Some(&parent.event_id),
        );
        entry.sender = reader.into();
        entry.verified_device = false;
        history.push(entry);
    }
    let request = message("authorized", "Answer the reader", Some(&parent.event_id));
    runner
        .ingest_with_context(&request, &snapshot, Some(&history), 101)
        .await
        .unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    let observations = agent.observations.lock().unwrap();
    let prompt = &observations.prompts[0];
    let json: serde_json::Value =
        serde_json::from_str(prompt.split_once("\n\n").unwrap().1).unwrap();
    assert_eq!(json["history"].as_array().unwrap().len(), 206);
    assert_eq!(json["history"][0]["sender"], reader);
    assert_eq!(json["history"][0]["body"], parent.body);
    assert_eq!(json["history"][205]["body"], "detail 204");
    assert_eq!(json["authorized_request"]["body"], "Answer the reader");
    assert_eq!(runner.bridge.store.count_runs().unwrap(), 1);
}

#[test]
fn context_rejects_cross_room_and_cross_thread_and_filters_unknown_readers() {
    let config = config();
    let request = message("request", "work", Some("$parent"));
    let mut parent = message("parent", "parent data", None);
    parent.room_id = "!other:example.invalid".into();
    assert!(context::prompt(&config, &request, &[parent.clone()]).is_err());
    parent.room_id = request.room_id.clone();
    parent.event_id = "$unrelated".into();
    assert!(context::prompt(&config, &request, &[parent.clone()]).is_err());
    parent.event_id = "$parent".into();
    parent.sender = "@unknown:example.invalid".into();
    assert!(
        !context::prompt(&config, &request, &[parent])
            .unwrap()
            .contains("parent data")
    );
}

async fn wait_permission(runner: &mut matrix_acp_bridge::runner::Runner) {
    loop {
        if matches!(next(runner).await, AgentEvent::Permission { .. }) {
            break;
        }
    }
}

#[tokio::test]
async fn automatic_policy_approves_offered_tool_option_without_chat_roundtrip() {
    let (mut runner, agent) = fixture(Behavior::Approval);
    runner.bridge.config.rooms[0].tool_approval = ToolApproval::Automatic;
    runner
        .ingest(&message("one", "work", None), &room(), 100)
        .await
        .unwrap();
    wait_permission(&mut runner).await;
    assert!(
        !runner
            .bridge
            .store
            .pending()
            .unwrap()
            .iter()
            .any(|o| o.body.contains("Approval requested"))
    );
    runner.tick(102).await.unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    assert!(agent.observations.lock().unwrap().decisions[0].contains("permit-once"));
}

#[tokio::test]
async fn allow_thread_resolves_pending_and_future_requests_and_can_be_revoked() {
    let (mut runner, agent) = fixture(Behavior::Approval);
    runner
        .ingest(&message("one", "work", None), &room(), 100)
        .await
        .unwrap();
    wait_permission(&mut runner).await;
    let mut unauthorized = message("outsider", "!bridge allow-thread", Some("$one"));
    unauthorized.sender = "@reader:example.invalid".into();
    assert_eq!(
        runner
            .ingest(&unauthorized, &room(), 101)
            .await
            .unwrap()
            .disposition,
        Disposition::Denied
    );
    runner.tick(102).await.unwrap();
    assert!(agent.observations.lock().unwrap().decisions.is_empty());

    runner
        .ingest(
            &message("allow", "!bridge allow-thread", Some("$one")),
            &room(),
            103,
        )
        .await
        .unwrap();
    runner.tick(104).await.unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    runner
        .ingest(&message("two", "more work", Some("$one")), &room(), 105)
        .await
        .unwrap();
    wait_permission(&mut runner).await;
    runner.tick(106).await.unwrap();
    assert_eq!(finish(&mut runner).await, RunStatus::Completed);
    assert_eq!(agent.observations.lock().unwrap().decisions.len(), 2);

    runner
        .ingest(
            &message("manual", "!bridge approvals manual", Some("$one")),
            &room(),
            107,
        )
        .await
        .unwrap();
    runner
        .ingest(&message("three", "more work", Some("$one")), &room(), 108)
        .await
        .unwrap();
    wait_permission(&mut runner).await;
    runner.tick(109).await.unwrap();
    assert_eq!(agent.observations.lock().unwrap().decisions.len(), 2);
    runner.shutdown(110).await.unwrap();
}

#[test]
fn automatic_approval_never_invents_allow_for_a_deny_only_request() {
    let mut bridge = bridge();
    bridge.config.rooms[0].tool_approval = ToolApproval::Automatic;
    let run_id = start(&mut bridge, "one");
    bridge
        .agent_event(
            AgentEvent::Permission {
                run_id,
                request_id: "deny-only".into(),
                title: "No allowed choice".into(),
                options: vec![PermissionOption {
                    id: "deny".into(),
                    label: "Deny".into(),
                    kind: "reject_once".into(),
                }],
            },
            101,
        )
        .unwrap();
    assert!(bridge.automatic_approvals(102).unwrap().is_empty());
    assert!(
        bridge
            .store
            .pending()
            .unwrap()
            .iter()
            .any(|o| o.body.contains("Approval requested"))
    );
}

#[test]
fn explicit_account_operator_can_work_without_sas_but_unknown_sender_cannot() {
    let mut bridge = bridge();
    let teammate = "@teammate:example.invalid";
    bridge.config.rooms[0].audience.insert(teammate.into());
    bridge.config.rooms[0].operators.insert(teammate.into());
    let mut snapshot = room();
    snapshot.members.insert(teammate.into());
    let mut request = message("teammate", "investigate", None);
    request.sender = teammate.into();
    request.verified_device = false;
    assert_eq!(
        bridge.handle(&request, &snapshot, 100).unwrap().disposition,
        Disposition::Denied
    );
    bridge.config.rooms[0].operator_trust = matrix_acp_bridge::config::OperatorTrust::Account;
    request.known_sender_device = false;
    assert_eq!(
        bridge.handle(&request, &snapshot, 101).unwrap().disposition,
        Disposition::Denied
    );
    request.known_sender_device = true;
    assert_eq!(
        bridge
            .handle(&request, &snapshot, 102)
            .unwrap()
            .effects
            .len(),
        1
    );
}

#[cfg(feature = "matrix")]
#[test]
fn account_trust_rejects_unknown_insecure_mismatched_and_changed_identities() {
    use matrix_acp_bridge::matrix::account_device_is_known;
    use matrix_sdk::deserialized_responses::{
        DeviceLinkProblem::*, VerificationLevel::*, VerificationState::*,
    };
    assert!(account_device_is_known(&Unverified(UnverifiedIdentity)));
    assert!(account_device_is_known(&Unverified(UnsignedDevice)));
    for state in [
        Unverified(None(MissingDevice)),
        Unverified(None(InsecureSource)),
        Unverified(MismatchedSender),
        Unverified(VerificationViolation),
    ] {
        assert!(!account_device_is_known(&state));
    }
}

#[test]
fn room_membership_accepts_new_readers_without_reset_but_keeps_operator_gate() {
    let mut bridge = bridge();
    bridge.config.rooms[0].audience_policy =
        matrix_acp_bridge::config::AudiencePolicy::RoomMembership;
    let run_id = start(&mut bridge, "one");
    let mut changed = room();
    changed
        .members
        .insert("@new-teammate:example.invalid".into());
    assert!(
        bridge
            .reconcile_room(&config().rooms[0].room_id, &changed)
            .unwrap()
            .is_empty()
    );
    let outbound = bridge.store.pending().unwrap().remove(0);
    assert!(bridge.may_deliver(&outbound, &changed).unwrap());
    let mut request = message("new-person", "work", Some("$one"));
    request.sender = "@new-teammate:example.invalid".into();
    assert_eq!(
        bridge.handle(&request, &changed, 101).unwrap().disposition,
        Disposition::Denied
    );
    bridge
        .agent_event(
            AgentEvent::Finished {
                run_id,
                status: RunStatus::Completed,
            },
            102,
        )
        .unwrap();
    let before = bridge.config.binding_fingerprint(&request.room_id);
    bridge.config.rooms[0]
        .operators
        .insert(request.sender.clone());
    assert_eq!(before, bridge.config.binding_fingerprint(&request.room_id));
    assert_eq!(
        bridge
            .handle(&request, &changed, 103)
            .unwrap()
            .effects
            .len(),
        1
    );
    changed.members.remove(&request.sender);
    assert_eq!(
        bridge
            .handle(
                &message("removed", "work", None),
                &RoomSnapshot {
                    members: std::collections::BTreeSet::new(),
                    ..changed
                },
                104
            )
            .unwrap()
            .disposition,
        Disposition::Denied
    );
}
