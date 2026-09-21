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

#[test]
fn timer_between_streaming_chunks_does_not_split_a_short_reply() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    text(&mut bridge, &run, "Thanks,");
    bridge.flush_progress(102).unwrap();
    assert!(replies(&bridge).is_empty());
    text(&mut bridge, &run, " Mark! 🫡");
    bridge.flush_progress(108).unwrap();
    assert!(replies(&bridge).is_empty());
    finish_run(&mut bridge, &run);
    assert_eq!(replies(&bridge), vec!["Thanks, Mark! 🫡"]);
    bridge.flush_progress(120).unwrap();
    assert_eq!(replies(&bridge), vec!["Thanks, Mark! 🫡"]);
}

#[test]
fn tool_boundary_delivers_complete_progress_before_the_tool_and_final_reply() {
    let mut bridge = bridge();
    let run = start(&mut bridge, "one");
    text(&mut bridge, &run, "I'll check ");
    bridge.flush_progress(102).unwrap();
    text(&mut bridge, &run, "the tests.");
    bridge
        .agent_event(
            AgentEvent::Tool {
                run_id: run.clone(),
                title: "Run tests".into(),
            },
            103,
        )
        .unwrap();
    assert_eq!(
        replies(&bridge),
        vec!["I'll check the tests.", "Working: Run tests"]
    );
    text(&mut bridge, &run, "All ");
    bridge.flush_progress(104).unwrap();
    text(&mut bridge, &run, "passed.");
    finish_run(&mut bridge, &run);
    assert_eq!(
        replies(&bridge),
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
