# Matrix ACP Bridge

Bring your own coding agent into a Matrix room. Mention its bot account to start work; it replies in a thread, and follow-up messages in that thread resume the same agent session. Written in Rust using the official Matrix and Agent Client Protocol SDKs.

**Early working prototype.** Encrypted mentions, threaded answers, and contextual follow-ups across a bridge restart have been exercised with stock Element and Codex ACP on Linux. Grok Build has also been verified with encrypted threads, native non-interrupting steering, session continuation and Matrix MCP tools. Other ACP adapters remain configurable. See [compatibility and limits](docs/COMPATIBILITY.md).

- Normal bot accounts on a compatible hosted or self-hosted Matrix server. No Matrix administrator token or server modification.
- Your choice of ACP stdio agent, optional permission mode, workspace and credentials.
- Encrypted messages, explicit operators and room audiences, optional cross-verification, and separate sessions per thread or room.
- Optional manual or automatic tool approval, same-session steering, persistent sessions and a durable inbox/outbox.
- Matrix history search with sender/date filters, full-thread reading, and image/file attachments.
- Status reactions: 👀 accepted, ✅ completed, ❌ failed, 🛑 cancelled, ⚠️ interrupted. Once a terminal reaction is delivered, the bot removes its 👀 from that message. Answers stay in the thread.

No dependency on Tailscale, a particular cloud, Bitwarden, SecretSpec, or Codex. The worker needs outbound HTTPS to Matrix and its agent's services. Federation does not need to be disabled. There is no inbound ACP listener.

## Quickstart

Use a dedicated development worker with only the access you want the agent to have. An agent can use whatever files and credentials its OS account can read. For a persistent service under the existing worker account, use the [Linux service guide](docs/LINUX-SERVICE.md).

### 1. Prepare a bot, room and agent

- Create an ordinary Matrix account for the bot on your chosen server. This version uses **password login**; browser-only SSO/OIDC enrollment is not implemented.
- Create an **encrypted** room and invite the bot. Copy its room ID from Element's **Room settings → Advanced**. Record the Matrix IDs of teammates allowed to direct the agent. Matrix room membership controls readership.
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

`enroll` asks for the bot password locally, creates its persistent encrypted device store and joins only the configured invitations. The wizard and example use `operator_trust = "account"`: adding a teammate’s Matrix ID to `operators` is enough. No per-person bot DM or emoji ceremony is required. Messages must still come from a known Matrix sender device without an SDK trust violation, in the configured encrypted room. Reading replies also works on unverified devices unless blocked.

For workers that require independent identity verification, use `operator_trust = "verified"` and run `matrix-acp-bridge verify '@you:example.org'` for each operator. Keep Element open on a verified device, select that device, compare the emojis and type `MATCH` only if they match. Both identity and sending device must be cross-verified. Configs that omit `operator_trust` keep this stricter legacy behavior.

Keep the bot’s state directory: recreating it creates a different device and loses stored sessions/keys. See [troubleshooting](docs/TROUBLESHOOTING.md) if enrollment or delivery fails.

### 4. Start and talk

```sh
./target/release/matrix-acp-bridge run
```

Wait for the initial sync, then send a **real Matrix mention pill** for the bot in the room, such as “@my-agent please explain this project's tests.” It reacts 👀, answers in a thread, and reacts ✅ when finished. Reply **in that thread** to continue; another mention is not required there. Send a new room mention to start a separate thread/session.

Messages predating the first run's sync baseline do not start work. On the first request in a thread, the bridge supplies its parent and all earlier decryptable text replies, including messages from permitted readers who are not operators. It paginates the whole thread without a hidden message limit. History is labeled as conversation data; only an authorized request starts work. A new room-level mention receives nearby preceding context (a 40-event Matrix context window), plus an explicit reply target when present. Image, file, audio and video messages include attachment descriptors; the agent can retrieve them using `matrix_attachment`. Images are returned as image content; other files are downloaded for the agent’s own tools. Message edits remain out of scope.

The first ACP input includes a short introduction and that initial history. Follow-ups reuse the same agent session and contain only new messages, normally one line such as `@alice:example.org: What about the other option?`. Any intervening reader messages are included once as `[context]` lines. The bridge does not repeat the introduction, old history or the agent's own replies. This bookkeeping survives bridge restarts and upgrades from the earlier full-context format. The introduction is ordinary ACP input, not a system-role override; it asks the agent to use its judgment about responding, adjusting its work or continuing without a reply.

Streamed reply fragments are combined until turn completion or a tool, permission or continuation boundary. A timer never posts half a sentence as a separate message. Progress text before a tool can still appear while work is running.

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

Use `!bridge allow-thread` to approve the pending tool request and subsequent tool requests in this conversation without copying request IDs. It persists across restarts. `!bridge approvals manual` restores individual approvals. To opt an isolated worker room into automatic approval from the start, set `tool_approval = "automatic"` in its `[[rooms]]` policy (default: `"manual"`). Automatic decisions prefer an actual `allow_once` choice, falling back to an offered `allow_always` choice when necessary. They are recorded in the journal and do not change the adapter’s configured mode or grant OS access. Requests offering no allow choice cannot be approved automatically.

`operator_trust = "account"` trusts the homeserver’s authenticated device list and the explicit operator allowlist. `"verified"` additionally requires independently verified identities. Unknown or mismatched sender devices, insecure key origins and verification violations remain rejected in both modes.

`operators` may start/control work. With `audience_policy = "room_membership"` (wizard/example), Matrix membership controls who reads the room; there is no second reader allowlist. Adding readers or changing operators does not invalidate existing threads. Operators must still be joined members to send a command. **Threads share the room’s audience.** Give differently privileged agents their own bot accounts and isolated worker credentials. Separate ACP sessions do not isolate a shared filesystem or account.

For an additional fixed reader allowlist, use `audience_policy = "configured"` and list every reader plus the bot in `audience`. In this stricter mode, membership/config changes invalidate old bindings. Legacy configs that omit the field retain this behavior. See [all policies and current limitations](docs/POLICIES.md).

Use additional `[[rooms]]` entries for more rooms. `conversation = "thread"` is the default; `"room"` uses one session for the entire room. Changing the actual harness/workspace/credentials still requires a fresh session binding; changing ordinary channel access in room-membership mode does not.

A follow-up arriving during work steers the same ACP session: the bridge delivers only new messages using the configured non-interrupting strategy. Native steering reaches the running turn; the portable fallback waits for the turn to finish. It does not enqueue a separate job or ask you to resend. An end-of-turn race is recovered durably as a continuation. `max_concurrent_runs = 0` removes the worker-wide concurrency limit; positive values limit independent conversations. No hidden model-token or turn-duration limit is imposed.

## Matrix tools

The bridge automatically supplies five read tools to the ACP agent: `matrix_rooms`, `matrix_search`, `matrix_thread`, `matrix_context`, and `matrix_attachment`. Search supports text, sender ID, inclusive `after` and exclusive `before` dates/timestamps, a thread filter, and pagination. It scans all accessible channel history, not just messages seen since startup. Dates without times mean midnight UTC.

Set `message_delivery = "explicit"` in a `[[rooms]]` entry to publish only intentional `send_message_to_thread({"text":"..."})` calls. The bridge supplies the current thread; the model does not provide routing IDs. Assistant narration and final text stay in the ACP session. The tool returns Matrix event IDs after server acknowledgement, and the agent can continue working after sending. Status reactions and necessary manual approval requests remain visible. Long messages use multiple Matrix events; a receipt lists them all. Network retries reuse stored transaction IDs. A new model tool invocation is a new send, not an idempotent retry of an earlier invocation.

The first session input includes: “Your assistant output is not posted to Matrix. Use `send_message_to_thread` when you want to communicate with the people in this thread. You may continue working without sending a message.” In room-membership mode, changing delivery mode updates an existing session with this instruction once; later inputs remain just new messages. There is no automatic-text fallback if the model forgets to send. Omitting `message_delivery` retains automatic assistant-text forwarding for compatibility.

Tools can read the bot’s configured channels under current Matrix membership. They do not use administrator APIs. History older than the bot’s membership or without available encryption keys may be unavailable; results report undecryptable events rather than implying a complete search. Search is a case-insensitive substring scan, not a full-text index. Downloaded attachments remain in the private state directory’s `downloads` folder.

## Documentation and development

- [Linux service](docs/LINUX-SERVICE.md)
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

### Follow-ups and providers

Follow-ups do not interrupt tools. The portable default (`harness.steering = "after_turn"`) appends them after the current ACP prompt finishes. For Codex ACP, opt into `concurrent_prompt` to send follow-ups during the turn. For Grok Build use `grok_interject`; see the [Grok configuration](config/grok.toml) and [provider notes](docs/COMPATIBILITY.md#grok-build). `!bridge stop` remains an explicit cancellation.

`harness.mode` is optional. If you set it, the bridge requires that exact advertised ACP mode. Agents without ACP modes retain their own permission settings; Matrix tool approvals still follow the room policy.
