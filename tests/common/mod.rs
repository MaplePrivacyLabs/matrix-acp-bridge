#![allow(dead_code)]
use matrix_acp_bridge::{
    config::Config,
    core::Bridge,
    model::*,
    offline::{Behavior, FixtureAgent},
    runner::Runner,
    store::Store,
};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

pub fn config() -> Config {
    let mut config = Config::parse(include_str!("../../config/example.toml")).unwrap();
    // Exercise independent conversations concurrently; the operator example
    // conservatively defaults to one run for a shared coding workspace.
    config.max_concurrent_runs = 2;
    config
}
pub fn room() -> RoomSnapshot {
    RoomSnapshot {
        joined: true,
        encrypted: true,
        members: config().rooms[0].audience.clone(),
    }
}
pub fn message(id: &str, body: &str, root: Option<&str>) -> Incoming {
    Incoming {
        event_id: format!("${id}"),
        room_id: config().rooms[0].room_id.clone(),
        sender: "@owner:example.invalid".into(),
        body: body.into(),
        thread_root: root.map(str::to_owned),
        reply_to: None,
        mentions: BTreeSet::from([config().bot_user_id]),
        encrypted: true,
        verified_device: true,
    }
}
pub fn bridge() -> Bridge {
    let config = config();
    let store = Store::memory(&config.bot_user_id).unwrap();
    Bridge::new(config, store).unwrap()
}
pub fn start(bridge: &mut Bridge, id: &str) -> String {
    let handled = bridge
        .handle(&message(id, "Do fixture work", None), &room(), 100)
        .unwrap();
    let Effect::Start { run_id } = &handled.effects[0] else {
        panic!("expected start")
    };
    let run_id = run_id.clone();
    bridge.started(&run_id).unwrap();
    run_id
}
pub fn fixture(behavior: Behavior) -> (Runner, FixtureAgent) {
    let agent = FixtureAgent::new(behavior);
    let factory_agent = agent.clone();
    (
        Runner::new(bridge(), Arc::new(move |_| factory_agent.transport()))
            .with_clock(Arc::new(|| 101)),
        agent,
    )
}
pub async fn next(runner: &mut Runner) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(5), runner.next_update())
        .await
        .expect("fixture timed out")
        .unwrap()
}
pub async fn finish(runner: &mut Runner) -> RunStatus {
    loop {
        if let AgentEvent::Finished { status, .. } = next(runner).await {
            return status;
        }
    }
}
