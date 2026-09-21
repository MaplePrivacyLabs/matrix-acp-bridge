# Architecture

`MatrixAdapter -> Bridge + Store -> Runner -> ACP client -> selected harness`

The Matrix and ACP SDKs own their wire protocols. The bridge owns the meaning of an authorized request, its durable run state, and where replies belong. The transport factory is the substitution point for a fixture, a local stdio harness, or a later isolated-worker transport. The bridge is an ACP client; it does not need a new ACP server protocol.

## Source map

| Module | Responsibility |
|---|---|
| `config.rs` | Strict TOML validation and immutable binding fingerprint |
| `core.rs` | Authorization, triggers, room/thread routing, approvals, cancellation and delivery policy |
| `store.rs` | SQLite transactions, exclusive ownership, wire staging, inbox/outbox and recovery |
| `runner.rs` | Worker lifecycle, bounded event channels, control messages and shutdown |
| `acp.rs` | Official ACP client, capability negotiation, mode enforcement and scoped stdio transport |
| `matrix.rs` | Optional official Matrix SDK adapter and standard thread rendering |
| `offline.rs` | Deterministic SDK agent fixture, with no AI or tools |
| `live.rs` | Enrollment, cross-signing verification, token persistence and sync loop |
| `setup.rs` | Offline guided configuration |
| `trust.rs` | Conservative sender-cache refresh for the pinned Matrix SDK |
| `main.rs` | CLI dispatch, diagnostics and offline demos |

## Identity and authority

One configured Matrix account owns one journal and worker profile. Give each human's agent its own account, runtime credentials and state. Multiple profiles do not create isolation if their processes share access to the same files or credentials.

The policy checks exact Matrix user and room IDs, joined membership, configured audience, E2EE and verified sending devices. A room's membership defines its audience; threads do not change that audience. Mentions are triggers. Approval, stop and status messages pass the same checks as work requests.

Bindings include bot ID, room ID, room-or-thread key, a hash of the configuration, and a hash of the current joined audience. Any binding mismatch blocks reuse and delivery. Start a new conversation after a deliberate policy change; migration/reset tooling is not yet implemented. In room mode, such changes need an explicit administrative migration before resuming that room's existing binding.

The roster check cannot retract already delivered plaintext, old keys or files read by a model. Room membership changes have unavoidable network/in-flight races. Each room's authorized audience must be permitted to see the data accessible to that worker. System prompts are not a confidentiality boundary.

## Delivery and crash behavior

Inbound event identity and run creation are recorded in one transaction. A partial unique index prevents concurrent runs in one conversation. Approval choice consumption is transactional and scoped to the active run/conversation; invalid choices do not consume it. Cancellation enters a distinct state and rejects later permission/output events until the worker settles.

The Matrix adapter first fetches the raw `/sync` response using the official client request API and stages permitted-room wire events in the application journal. It then performs a zero-wait SDK sync from the same cursor for SDK-owned room/key processing and decrypts the staged events with the SDK. This extra sync request is intentional: the SDK suppresses a repeated `next_batch`, so relying only on its processed response leaves a crash window. Application checkpoint and staged-batch removal are atomic after durable event admission.

First sync establishes a baseline without running historical prompts. Later timeline gaps and undecryptable events hold the staged batch instead of advancing past it. A limited timeline can backfill up to 500 events to the last checkpointed room event. Missing continuity retains the staged cursor; richer recovery UX is still needed. The current implementation favors a visible stop over silently dropping work.

Outbound chunks have persistent transaction IDs. Uncertain HTTP sends retry with the same ID; successfully acknowledged sends are marked delivered. This does not give exactly-once external agent side effects. Restart marks queued/running/waiting/cancelling runs interrupted, cancels approvals and requires human review before new instructions.

The journal contains decrypted prompts and output; private directory permissions are not disk encryption. The Matrix crypto store separately requires a passphrase. Backups and retention must protect both stores.

## ACP behavior

The client uses stable protocol-v1 entrypoints and advertised capabilities. It loads a stored session when supported, otherwise uses advertised resume support, otherwise refuses to pretend context survived. It requires an advertised permission mode and an acknowledged mode-setting request before the prompt. Client-provided filesystem and terminal capabilities remain disabled.

Permission callbacks release the SDK dispatch loop while waiting for a human. Blocking that loop would prevent cancellation and other incoming traffic. Responses preserve actual offered option IDs; unknown choices never become approval. A cancellation request also closes approval admission.

Normal model turns have no artificial duration or output-token cap. Initialization/session/mode handshakes have 30-second deadlines. Explicit cancellation has a five-second grace period, after which the run is marked interrupted and the transport closes. The subprocess transport clears inherited environment variables and owns a Unix process group. It provides lifecycle hygiene, not a sandbox.

## References inspected

- [Matrix Rust SDK](https://github.com/matrix-org/matrix-rust-sdk), pinned crate `0.19.1`.
- [ACP Rust SDK](https://github.com/agentclientprotocol/rust-sdk), pinned crate `2.2.0`.
- [Paseo ACP provider](https://github.com/getpaseo/paseo/blob/4092c2926500d51af6510bbe295591af0dfe52f7/packages/server/src/server/agent/providers/acp-agent.ts): lifecycle, permissions, replay and capability behavior.
- [Goose dependencies](https://github.com/aaif-goose/goose/blob/2090ad1c65ddb39497601a936a9fe17d66254bfe/Cargo.toml): current use of the official Rust ACP SDK.
- [OpenCode Chat Bridge](https://github.com/ominiverdi/opencode-chat-bridge/tree/6535cc50d08ed5309674c12d5e2bc43f273db304): Matrix conversation/thread mapping reference.

This is an independent implementation using the SDKs above, not a fork of a Matrix client, homeserver, or reference bridge.

## Verified-sender cache compatibility

The pinned Matrix SDK can retain `SenderUnverified` metadata for a Megolm session received before cross-signing verification. Under the exclusive worker lock, before the network client opens, the bridge recalculates that metadata through the SDK. It only updates a direct, non-imported, non-forwarded session when the SDK now reports `SenderVerified` for the same user, device, exact master key and matching session/device signing and sender keys. Unknown senders, local-only trust and identity changes are not promoted. Normal event admission still requires the SDK's verified state. Re-evaluate this compatibility code when updating the SDK.

Session-refresh callbacks are installed before restoring the network client and persist tokens with private, atomic file replacement. Diagnostics share that restore path and the exclusive state lock.
