# Matrix ACP Bridge

Bring your own coding agent into a Matrix room. Mention its bot account to start work; it replies in a thread, and follow-up messages in that thread resume the same agent session. Written in Rust using the official Matrix and Agent Client Protocol SDKs.

**Early working prototype.** Encrypted mentions, threaded answers, and contextual follow-ups across a bridge restart have been exercised with stock Element and Codex ACP on Linux. Other ACP adapters are configurable, but have not yet been verified end to end. See [compatibility and limits](docs/COMPATIBILITY.md).

- Normal bot accounts on a compatible hosted or self-hosted Matrix server. No Matrix administrator token or server modification.
- Your choice of ACP stdio agent, permission mode, workspace and credentials.
- Encrypted messages, explicit operators and room audiences, optional cross-verification, and separate sessions per thread or room.
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

### 3. Enroll

```sh
./target/release/matrix-acp-bridge enroll
```

`enroll` asks for the bot password locally, creates its persistent encrypted device store and joins only the configured invitations. The wizard and example use `operator_trust = "account"`: adding a teammate’s Matrix ID to `operators` and `audience` is enough. No per-person bot DM or emoji ceremony is required. Messages must still come from a known Matrix sender device without an SDK trust violation, in the configured encrypted room. Reading replies also works on unverified devices unless blocked.

For workers that require independent identity verification, use `operator_trust = "verified"` and run `matrix-acp-bridge verify '@you:example.org'` for each operator. Keep Element open on a verified device, select that device, compare the emojis and type `MATCH` only if they match. Both identity and sending device must be cross-verified. Configs that omit `operator_trust` keep this stricter legacy behavior.

Keep the bot’s state directory: recreating it creates a different device and loses stored sessions/keys. See [troubleshooting](docs/TROUBLESHOOTING.md) if enrollment or delivery fails.

### 4. Start and talk

```sh
./target/release/matrix-acp-bridge run
```

Wait for the initial sync, then send a **real Matrix mention pill** for the bot in the room, such as “@my-agent please explain this project's tests.” It reacts 👀, answers in a thread, and reacts ✅ when finished. Reply **in that thread** to continue; another mention is not required there. Send a new room mention to start a separate thread/session.

Messages predating the first run's sync baseline do not start work. When an operator requests work in a thread, the bridge automatically supplies its parent and all earlier decryptable text replies, including messages from permitted readers who are not operators. It paginates the whole thread without a hidden message limit. History is labeled as conversation data; only the current authorized request starts work. A new room-level mention receives nearby preceding context (a 40-event Matrix context window), plus an explicit reply target when present. Attachments and edits are not yet agent inputs.

## Permissions and conversations

Inside the established thread:

```text
!bridge status
!bridge stop
!bridge approve <request-id> <offered-option-id>
!bridge allow-thread
!bridge approvals manual
```

Approval choices come from the adapter and include its offered denial option. Only an authorized operator meeting the room’s configured sender-trust policy may answer. Requests expire and are cancelled on restart. Reactions cannot grant approval. Which operations ask for approval depends on the adapter and its configured mode; the bridge is not an OS sandbox.

Use `!bridge allow-thread` to approve the pending tool request and subsequent tool requests in this conversation without copying request IDs. It persists across restarts. `!bridge approvals manual` restores individual approvals. To opt an isolated worker room into automatic approval from the start, set `tool_approval = "automatic"` in its `[[rooms]]` policy (default: `"manual"`). Automatic decisions select only an actual `allow_once` option, are recorded in the journal, and do not change the adapter's configured mode or grant OS access. Requests without that option still ask a human.

`operator_trust = "account"` trusts the homeserver’s authenticated device list and the explicit operator allowlist. `"verified"` additionally requires independently verified identities. Unknown or mismatched sender devices, insecure key origins and verification violations remain rejected in both modes.

`operators` may start/control work. With `audience_policy = "room_membership"` (wizard/example), Matrix membership controls who reads the room; there is no second reader allowlist. Adding readers or changing operators does not invalidate existing threads. Operators must still be joined members to send a command. **Threads share the room’s audience.** Give differently privileged agents their own bot accounts and isolated worker credentials. Separate ACP sessions do not isolate a shared filesystem or account.

For an additional fixed reader allowlist, use `audience_policy = "configured"` and list every reader plus the bot in `audience`. In this stricter mode, membership/config changes invalidate old bindings. Legacy configs that omit the field retain this behavior. See [all policies and current limitations](docs/POLICIES.md).

Use additional `[[rooms]]` entries for more rooms. `conversation = "thread"` is the default; `"room"` uses one session for the entire room. Changing the actual harness/workspace/credentials still requires a fresh session binding; changing ordinary channel access in room-membership mode does not.

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
