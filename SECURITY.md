# Security model

This is an early prototype, not an independently audited agent sandbox. Run it on a worker whose filesystem, credentials and network access match the work that room's audience is allowed to see. ACP permission prompts and model instructions cannot replace OS/account isolation.

## Boundaries

- The Matrix bot is an ordinary account. It receives no homeserver administrator token, shell login or private-network entitlement from the bridge.
- Inbound work requires an encrypted message from an explicitly configured operator satisfying the room’s sender-trust policy in a configured room with an allowed joined audience. Replies are checked against policy and current audience before sending. Threads do not create separate room membership.
- Outgoing reply keys are shared with room members' unblocked devices, including unverified or unsigned devices. This allows ordinary team reading without per-reader SAS verification. It also means reading relies on the homeserver's authenticated device list and room membership; a malicious homeserver-injected device is not excluded merely for lacking cross-signing. Command admission separately enforces the operator allowlist and sender-trust policy.
- With `audience_policy = "configured"`, a config/audience fingerprint binds each conversation to its original policy and roster. With `"room_membership"`, Matrix membership controls readership, membership changes are accepted, and session bindings track the worker runtime rather than operator/reader lists. Adding a reader makes thread history visible according to Matrix room history rules; it is not a separate bridge approval.
- The agent receives an explicit environment, not the bridge's inherited environment. The normal Linux service uses the existing worker account. An unrestricted agent under that account can read the bridge state too; use an isolated VM with the intended access. An optional split-identity recipe is available.
- Treat repository files, tool results and other participants' text as potentially hostile input. The bridge does not eliminate prompt injection. Anyone authorized to operate an agent can direct the authority already granted to that worker.
- Session tokens, crypto store and decrypted journal are local sensitive data. The crypto store is encrypted with a local key; that key is stored alongside the state. File permissions alone do not protect against root, disk theft or an unrestricted process under the same UID. Protect backups and the worker's disk.
- Federation and hosted Matrix are compatible with the design, but E2EE does not hide membership, timing, reaction metadata or endpoint access. Verify your client/devices and understand your provider's policies.

## Permissions and recovery

Approval answers must come from a configured operator satisfying sender-trust policy in the same conversation and match an actual unexpired option ID. A reaction cannot approve. Restart cancels pending approvals and marks unfinished work interrupted. This does not roll back external side effects. Use narrowly scoped repository/API credentials even in an approved agent mode.

`operator_trust = "account"` (the wizard/example policy) accepts configured operators’ SDK-linked known devices with unverified identities or unsigned devices, relying on the homeserver’s authenticated device list. It does not accept unknown devices, insecure-source keys, sender mismatches, or verification violations. This provides normal account-allowlist onboarding; it is not protection against a malicious homeserver substituting account devices. Use `operator_trust = "verified"` for independently verified sending devices and cross-signing identities. Older configs that omit the field retain verified trust; changing it deliberately changes the binding fingerprint.

Authorized requests include text from the same thread, including a reader’s parent message. Context is untrusted data, not an additional command admission path. Never treat context instructions as permissions. Attachment metadata is included and read-only Matrix tools retrieve attachment content. Edits are not treated as new instructions.

Tool approval defaults to manual. Operators may opt a conversation into automatic approval with `!bridge allow-thread`; administrators may configure a room with `tool_approval = "automatic"`. The bridge journals each selected allow option and keeps mode, caller/session, expiration, cancellation, audience and operator checks. It never fabricates an allow option. Automatic policy prefers `allow_once`, but accepts an offered `allow_always` option when that is the backend’s only allow choice. These policies can authorize any tool action offered by that worker, so the worker’s OS and credentials must enforce its intended access.

Sender-cache reconciliation for the pinned SDK never substitutes a local trust bit for a verified cross-signing identity. Its exact key-matching rules are described in [architecture](docs/ARCHITECTURE.md) and tested against identity changes and imported/forwarded sessions.

The local Matrix MCP server shares the running SDK client over a private Unix socket. It offers read tools only, checks configured channel access and current membership, and does not expose encryption keys in responses. A model may search any channel configured for its worker. This scope is deliberately broader than the originating thread.

## Reporting

Use this repository's **Security → Report a vulnerability** private reporting flow for suspected security issues. Do not post credentials, state databases, private Matrix IDs/messages, or exploit details involving a live deployment in public issues. Sanitized reliability or interoperability reports may use ordinary issues.
