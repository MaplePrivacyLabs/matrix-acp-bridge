# Troubleshooting

Use the same `--config /absolute/path/config.toml` throughout. For the Linux service layout, run commands as `matrix-acp-bridge` and stop the service first when inspecting its state. `check` only parses configuration; a passing check is not proof of authentication or connectivity.

| Symptom | Check |
|---|---|
| No reaction | Wait for `Ready`, send a new real Matrix mention pill, and check room/operator/audience IDs. The first sync intentionally skips history. |
| 👀 followed by ❌ | Run `doctor` under the bridge identity. Check agent login, HOME, runtime PATH, absolute executable, workspace and advertised mode. |
| `cannot start scoped ACP worker` | The executable must exist and be executable. For the service layout, verify the fixed launcher and exact sudoers entry; don't grant general sudo. |
| `required permission mode` / mode unavailable | Use a mode reported by `doctor`; adapter mode names differ. The bridge will not silently choose another one. |
| `UnverifiedIdentity` after matching emojis | The human's active Element device must itself have valid cross-signing. Unlock/verify it in Element, rerun `verify`, then restart the bridge. Do not disable verified-sender admission. |
| Human can't decrypt a bot reply | Verify that human's cross-signing identity/device with the bot too. Keys are shared only with trusted devices. A reply sent before verification may require key recovery or a fresh test. |
| Unknown joined member / held output | Review the actual room roster against `audience`. Update deliberately, restart, and start a new thread. Threads are visible to the whole room. |
| `another worker owns this state directory` | Stop the other process/service. Never copy the same device store into a second running bridge. |
| DNS/HTTPS timeout | Test the configured public homeserver from the worker. The bot needs ordinary HTTPS, not a private administrator endpoint. Investigate local network policy without granting the bot admin-network access. |
| Expired/invalid Matrix token | Stop the bridge and investigate the existing device/session. Do not delete state or reenroll blindly; that can lose keys and bindings. Token refresh is persisted automatically when supported. |
| Timeline gap / undecryptable event | The staged batch is retained. Restore connectivity/keys and restart; don't advance cursors manually. Gaps beyond the bounded backfill need additional recovery work. |
| No follow-up context | Reply in the existing thread, verify the adapter supports persistent load/resume, and preserve its own session files. A new mention at room level creates a new thread. |

## Diagnostics

```sh
matrix-acp-bridge check --config config.toml
matrix-acp-bridge doctor --config config.toml
matrix-acp-bridge status --config config.toml
```

`doctor` launches the adapter but sends no prompt. `status` connects to Matrix and prints identity/device trust plus recent event metadata, without message bodies or tokens. It shares the normal token-refresh and locking path.

For a single-room configuration, `inspect-event --config config.toml '$event-id'` prints a selected decrypted message locally. Its output is private. `run --config config.toml --retry-event '$event-id'` can explicitly reconsider an event rejected before it was admitted. This goes through the normal policy and deduplication checks; it does not rewind sync or rerun an already admitted failed/completed task. Send a new human instruction to retry a failed run.

## Recovery and backups

The state directory contains the Matrix login session, crypto store/key and a SQLite journal with decrypted prompts/output. Protect all of it. The agent separately owns its session history, repository changes and provider credentials. A homeserver backup alone does not preserve those worker-side files.

Stop the service for a consistent backup. Store it encrypted/access-controlled and test a restore without letting the original and restored devices connect at once. On startup, unfinished runs are marked interrupted and approvals are cancelled. Review possible filesystem/external effects before issuing a new request; exactly-once Matrix delivery does not imply exactly-once tool actions. Do not solve recovery by silently running historical messages again.

For bug reports, provide versions, sanitized config, the failing command and redacted errors. Never upload the state directory, bot password, session JSON, store key, agent authentication or private conversation contents.
