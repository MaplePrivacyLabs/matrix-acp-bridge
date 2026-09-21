# Policies and remaining restrictions

Use `matrix-acp-bridge check --config config.toml --json` to inspect effective access settings and binding hashes without connecting to Matrix or an agent. It does not print the child environment or credentials.

## Access settings

| Setting | Behavior |
|---|---|
| `rooms` | The bot only processes explicitly configured rooms. |
| `operators` | Exact Matrix accounts allowed to start, stop or approve work. They must currently be room members. Other people’s text can supply context without starting work. |
| `operator_trust = "account"` | Known Matrix sender device plus operator allowlist; no per-person DM/SAS verification. Wizard/example default. |
| `operator_trust = "verified"` | Additionally requires cross-verified identity/device. Legacy default when omitted. |
| `audience_policy = "room_membership"` | Matrix controls readers. No second reader allowlist or conversation reset when membership/operators change. Wizard/example default. |
| `audience_policy = "configured"` | Additional `audience` allowlist and immutable roster/config bindings. Unexpected readers or binding changes hold work/output. Legacy default when omitted. |
| `tool_approval = "automatic"` | Select offered `allow_once` tool choices automatically and journal decisions. Does not change the ACP mode or worker OS permissions. |
| `tool_approval = "manual"` | Relay requests to the conversation. Default when omitted. `!bridge allow-thread` opts in for a conversation; `!bridge approvals manual` turns it off there. |
| `harness.mode` | Must be advertised and applied by the adapter; an unsolicited change stops the run. Tool sandbox/access behavior is adapter-specific. |
| `conversation` | `thread` creates separate sessions per thread; `room` shares a session throughout the room. Workspace/credentials are still shared. |
| `max_concurrent_runs` | 1–64 active runs per worker, default 2; example/wizard uses 1 for a shared coding workspace. Only one active run per conversation. |
| `approval_ttl_seconds` | Manual decisions expire after 1–3600 seconds, default 300. A backend request without an `allow_once` option still needs manual choice. |

Changing the harness executable, workspace, credentials or mode changes the runtime binding and needs an explicit fresh session or administrative migration. In room-membership mode, access-list and approval-policy changes apply without invalidating a session.

## Fixed behavior and implementation limits

- **Encrypted rooms and HTTPS only.** Sender/device mismatch, unknown/insecure key origin, or a verification violation cannot authorize work. Plaintext and forged-sender inputs are rejected even in account-trust mode.
- **Text inputs.** The full prior text thread, including its parent and permitted non-operators’ messages, is fetched automatically. Thread pagination has no hidden message-count cutoff. Attachments, image input and edits are not yet supported. New room-level mentions receive a 40-event Matrix context window and any explicit reply target.
- **Triggers.** A real bot mention starts a thread; approved follow-ups in an established bot thread continue it. Ordinary unrelated room chatter does not start work. Initial startup establishes a sync baseline without executing historical requests.
- **Busy workers reject instead of queueing.** Messages arriving during an active conversation/run limit ask the sender to resend later. There is no mid-turn steering or durable work queue yet.
- **Restart interrupts unfinished work.** Pending permissions are cancelled; work is not replayed automatically. Existing completed sessions remain resumable. Exactly-once external tool effects are not guaranteed.
- **No implicit context fallback.** If thread history cannot be fetched/decrypted, the request remains staged instead of asking the agent to guess or search another service. Timeline continuity recovery is currently capped at 500 events; larger gaps need operator recovery.
- **Network recovery is limited.** Sync/delivery steps have 15-second deadlines, ACP handshakes have 30-second deadlines, and five consecutive Matrix step failures stop the worker. A service manager may restart it. Explicit cancellation has a five-second grace period. Normal agent turns have no artificial model-token or runtime cap.
- **ACP compatibility.** Stable protocol v1, advertised modes and load/resume are required. Agents must provide their own filesystem and terminal tools; the bridge does not expose client filesystem/terminal capabilities. No interactive provider login is performed in Matrix.
- **Enrollment.** Matrix password login only; browser-only OIDC/SSO enrollment is not implemented. One bot/device store must have a single active worker owner.
- **Output.** Text replies and status reactions are supported. Rich artifact upload is not. Replies remain encrypted and use standard Matrix threads. Tool activity can still produce progress text.
- **Isolation.** The bridge’s separate OS account, fixed launcher and agent account permissions remain the worker’s actual access boundary. Room/session separation does not isolate a shared filesystem. Environment filtering and chat approvals are not an OS sandbox.

These are current behaviors, not all permanent product requirements. Deployment policy should be explicit rather than making users discover restrictions through silent denials.
