mod common;
use common::*;
use matrix_acp_bridge::{core::Bridge, model::*};

fn text(bridge: &mut Bridge, run: &str, text: &str) {
    bridge
        .agent_event(
            AgentEvent::Text {
                run_id: run.into(),
                text: text.into(),
            },
            101,
        )
        .unwrap();
}
fn replies(bridge: &Bridge) -> Vec<String> {
    bridge
        .store
        .pending()
        .unwrap()
        .into_iter()
        .filter(|o| o.reaction.is_none())
        .map(|o| o.body)
        .collect()
}
fn finish_run(bridge: &mut Bridge, run: &str) {
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

#[tokio::test]
async fn timer_between_streaming_chunks_does_not_split_a_short_reply() {
    let (mut runner, _) = fixture(matrix_acp_bridge::offline::Behavior::Reply);
    let run = start(&mut runner.bridge, "one");
    text(&mut runner.bridge, &run, "Thanks,");
    runner.tick(102).await.unwrap();
    assert!(replies(&runner.bridge).is_empty());
    text(&mut runner.bridge, &run, " Mark! 🫡");
    runner.tick(108).await.unwrap();
    assert!(replies(&runner.bridge).is_empty());
    finish_run(&mut runner.bridge, &run);
    assert_eq!(replies(&runner.bridge), vec!["Thanks, Mark! 🫡"]);
    runner.tick(120).await.unwrap();
    assert_eq!(replies(&runner.bridge), vec!["Thanks, Mark! 🫡"]);
}

#[tokio::test]
async fn tool_boundary_delivers_complete_progress_before_the_tool_and_final_reply() {
    let (mut runner, _) = fixture(matrix_acp_bridge::offline::Behavior::Reply);
    let run = start(&mut runner.bridge, "one");
    text(&mut runner.bridge, &run, "I'll check ");
    runner.tick(102).await.unwrap();
    text(&mut runner.bridge, &run, "the tests.");
    runner
        .bridge
        .agent_event(
            AgentEvent::Tool {
                run_id: run.clone(),
                title: "Run tests".into(),
            },
            103,
        )
        .unwrap();
    assert_eq!(
        replies(&runner.bridge),
        vec!["I'll check the tests.", "Working: Run tests"]
    );
    text(&mut runner.bridge, &run, "All ");
    runner.tick(104).await.unwrap();
    text(&mut runner.bridge, &run, "passed.");
    finish_run(&mut runner.bridge, &run);
    assert_eq!(
        replies(&runner.bridge),
        vec!["I'll check the tests.", "Working: Run tests", "All passed."]
    );
}

#[test]
fn continuation_boundary_separates_responses_and_discards_empty_chunks() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    text(&mut bridge, &run, "First response.");
    bridge
        .agent_event(
            AgentEvent::Steered {
                run_id: run.clone(),
                event_id: "$followup".into(),
            },
            103,
        )
        .unwrap();
    text(&mut bridge, &run, "Second response.");
    finish_run(&mut bridge, &run);
    assert_eq!(
        replies(&bridge),
        vec!["First response.", "Second response."]
    );
    let empty = start(&mut bridge, "empty");
    text(&mut bridge, &empty, " \n");
    finish_run(&mut bridge, &empty);
    assert_eq!(replies(&bridge).len(), 2);
}

#[test]
fn crash_recovery_preserves_buffered_text_once_without_a_timer() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    text(&mut bridge, &run, "Buffered before crash.");
    assert!(replies(&bridge).is_empty());
    assert_eq!(bridge.store.recover(200).unwrap(), 1);
    let first = replies(&bridge);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0], "Buffered before crash.");
    assert!(first[1].contains("interrupted"));
    assert_eq!(bridge.store.recover(201).unwrap(), 0);
    assert_eq!(replies(&bridge), first);
}
