# Matrix ACP Bridge

Bring your own coding agent into a Matrix room. Mention its bot account to start work; it replies in a thread, and follow-up messages in that thread resume the same agent session. Written in Rust using the official Matrix and Agent Client Protocol SDKs.

**Early working prototype.** Encrypted mentions, threaded answers, and contextual follow-ups across a bridge restart have been exercised with stock Element and Codex ACP on Linux. Other ACP adapters are configurable, but have not yet been verified end to end. See [compatibility and limits](docs/COMPATIBILITY.md).

- Normal bot accounts on a compatible hosted or self-hosted Matrix server. No Matrix administrator token or server modification.
- Your choice of ACP stdio agent, permission mode, workspace and credentials.
- Verified encrypted messages, explicit operators and room audiences, and separate sessions per thread or room.
- Human approval requests in the conversation, cancellation, persistent sessions and a durable inbox/outbox.
- Status reactions: 👀 accepted, ✅ completed, ❌ failed, 🛑 cancelled, ⚠️ interrupted. Answers stay in the thread.

No dependency on Tailscale, a particular cloud, Bitwarden, SecretSpec, or Codex. The worker needs outbound HTTPS to Matrix and its agent's services. Federation does not need to be disabled. There is no inbound ACP listener.

## Quickstart

Use a dedicated development worker with only the access you want the agent to have. An agent can use whatever files and credentials its OS account can read. For separate bridge/agent identities and a persistent Linux service, use the [Linux service guide](docs/LINUX-SERVICE.md).

### 1. Prepare a bot, room and agent

- Create an ordinary Matrix account for the bot on your chosen server. This version uses **password login**; browser-only SSO/OIDC enrollment is not implemented.
- Create an **encrypted** room and invite the bot. Copy its room ID from Element's **Room settings → Advanced**. Record the Matrix IDs of everyone allowed to read the room, and which of them may direct the agent.
- Install an [ACP agent or adapter](https://agentclientprotocol.com/get-started/agents) on the worker and authenticate it under the OS identity that will run it. The bridge does not perform provider login. For the tested adapter, see [Codex ACP](https://github.com/agentclientprotocol/codex-acp) and our [compatibility notes](docs/COMPATIBILITY.md).

### 2. Build and configure

Install Rust using [rustup](https://rustup.rs/), plus your platform's C/C++ build tools and CMake. On Ubuntu: `sudo apt-get install build-essential cmake pkg-config`. The repository selects Rust 1.97.1. Nix is an optional alternative (`nix develop`).

```sh
git clone https://github.com/MaplePrivacyLabs/matrix-acp-bridge.git
cd matrix-acp-bridge
cargo build --locked --release --features matrix
./target/release/matrix-acp-bridge init
./target/release/matrix-acp-bridge doctor
```

`init` writes a private `config.toml` and never overwrites one. It asks for IDs, an ACP executable and arguments, workspace, mode, and the agent's HOME/PATH. No passwords are stored in that configuration. `doctor` starts the adapter, creates an empty session, checks its modes and ability to resume, then exits **without sending a prompt or connecting to Matrix**. If the mode is wrong, it lists available modes so you can correct the config.

Only explicitly configured environment variables reach the adapter. Include the runtime directory in PATH when an adapter needs Node, Python, or another runtime. Do not point the worker at a privileged administrator's HOME. See [the example config](config/example.toml) for the complete format.

### 3. Enroll and verify

```sh
./target/release/matrix-acp-bridge enroll
./target/release/matrix-acp-bridge verify '@you:example.org'
```

`enroll` asks for the bot password locally, creates its persistent encrypted device store and joins only the configured invitations. Keep Element open on a **verified** human device. `verify` lists your devices; choose that active device, accept the request in Element, compare the emojis on both screens, and type `MATCH` in the worker terminal **only if they match**. Repeat for each operator **and each additional person who should read the bot's replies**: outbound keys are shared only with trusted devices. Both the device and cross-signing identity must be verified; a local device trust flag alone is insufficient.

This is the trust handshake, not an administrator login. Keep the bot's state directory: recreating it creates a different device and loses the stored sessions/keys. See [troubleshooting](docs/TROUBLESHOOTING.md) if verification or delivery fails.

### 4. Start and talk

```sh
./target/release/matrix-acp-bridge run
```

Wait for the initial sync, then send a **real Matrix mention pill** for the bot in the room, such as “@my-agent please explain this project's tests.” It reacts 👀, answers in a thread, and reacts ✅ when finished. Reply **in that thread** to continue; another mention is not required there. Send a new room mention to start a separate thread/session.

Messages predating the first run's sync baseline do not start work. Ordinary room messages are not automatically sent to the agent. The initial prompt contains the triggering message, not the entire room's history; include or quote the context you want it to use. Attachments and edits are not yet agent inputs.

## Permissions and conversations

Inside the established thread:

```text
!bridge status
!bridge stop
!bridge approve <request-id> <offered-option-id>
```

Approval choices come from the adapter and include its offered denial option. Only an authorized verified operator in that conversation may answer. Requests expire and are cancelled on restart. Reactions cannot grant approval. Which operations ask for approval depends on the adapter and its configured mode; the bridge is not an OS sandbox.

`operators` may start/control work; `audience` lists all permitted room readers, including the bot. A newly joined reader outside that list blocks work and delivery. **Threads share the room's audience.** Give each differently privileged agent its own bot account, config, state directory and isolated worker credentials. Separate ACP sessions do not isolate a shared filesystem or account.

Use additional `[[rooms]]` entries for more rooms. `conversation = "thread"` is the default; `"room"` uses one session for the entire room. Changing policy or joined membership invalidates existing conversation bindings. Review the change, restart, and start a new thread; migration of an existing room-mode session is not implemented.

A busy conversation asks you to wait, or stop and resend. Mid-turn message queuing/steering is not implemented. No hidden model-token or turn-duration limit is imposed by the bridge.

## Documentation and development

- [Linux service and separate OS identities](docs/LINUX-SERVICE.md)
- [Agent compatibility and current limits](docs/COMPATIBILITY.md)
- [Troubleshooting and recovery](docs/TROUBLESHOOTING.md)
- [Architecture](docs/ARCHITECTURE.md) · [Security](SECURITY.md) · [Contributing](CONTRIBUTING.md)

Offline checks need neither credentials nor a live agent:

```sh
cargo fmt --check
cargo clippy --locked --all-features --all-targets -- -D warnings
cargo test --locked --all-features
cargo run --locked -- check --config config/example.toml
cargo run --locked -- demo conversation
cargo run --locked -- demo approval
cargo run --locked -- demo cancellation
cargo run --locked -- demo recovery
```

The demos use deterministic in-memory ACP fixtures. They do not contact Matrix or a model. MIT licensed.
