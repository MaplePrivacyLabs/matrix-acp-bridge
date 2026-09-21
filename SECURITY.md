# Security model

This is an early prototype, not an independently audited agent sandbox. Run it on a worker whose filesystem, credentials and network access match the work that room's audience is allowed to see. ACP permission prompts and model instructions cannot replace OS/account isolation.

## Boundaries

- The Matrix bot is an ordinary account. It receives no homeserver administrator token, shell login or private-network entitlement from the bridge.
- Inbound work requires an encrypted message from a verified, explicitly configured operator in a configured room with an allowed joined audience. Replies are checked against policy and current audience before sending. Threads do not create separate room membership.
- A config/audience fingerprint binds each conversation to its original policy. Changes block old bindings rather than reuse their agent context for a different audience.
- The agent receives an explicit environment, not the bridge's inherited environment. Use separate OS identities or workers to prevent it reading Matrix credentials. The optional Linux installer implements two identities and one fixed run-as command; it grants neither account general sudo.
- Treat repository files, tool results and other participants' text as potentially hostile input. The bridge does not eliminate prompt injection. Anyone authorized to operate an agent can direct the authority already granted to that worker.
- Session tokens, crypto store and decrypted journal are local sensitive data. The crypto store is encrypted with a local key; that key is stored alongside the state. File permissions alone do not protect against root, disk theft or an unrestricted process under the same UID. Protect backups and the worker's disk.
- Federation and hosted Matrix are compatible with the design, but E2EE does not hide membership, timing, reaction metadata or endpoint access. Verify your client/devices and understand your provider's policies.

## Permissions and recovery

Approval answers must come from a verified configured operator in the same conversation and match an actual unexpired option ID. A reaction cannot approve. Restart cancels pending approvals and marks unfinished work interrupted. This does not roll back external side effects. Use narrowly scoped repository/API credentials even in an approved agent mode.

Sender-cache reconciliation for the pinned SDK never substitutes a local trust bit for a verified cross-signing identity. Its exact key-matching rules are described in [architecture](docs/ARCHITECTURE.md) and tested against identity changes and imported/forwarded sessions.

## Reporting

Use this repository's **Security → Report a vulnerability** private reporting flow for suspected security issues. Do not post credentials, state databases, private Matrix IDs/messages, or exploit details involving a live deployment in public issues. Sanitized reliability or interoperability reports may use ordinary issues.
