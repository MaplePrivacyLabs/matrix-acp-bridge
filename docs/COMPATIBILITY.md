# Compatibility and current limits

## Matrix

The bridge is an ordinary client of the Matrix Client-Server API, built with Matrix Rust SDK **0.19.1**. The bot can live on a hosted or self-hosted server; no server admin API, custom DNS, VPN, federation restriction, or Element fork is required. Accounts and rooms can span servers, subject to those servers' normal policies.

Enrollment currently uses Matrix password login and cross-signing bootstrap. A server with only browser-based OIDC/SSO login needs additional enrollment support. There is no generic account-creation API: create the bot using your provider's usual signup/invite process. Use a dedicated account rather than repurposing an existing personal account with other sessions/keys.

Work rooms must be encrypted. Senders must be allowlisted operators and current members. Account trust uses known Matrix sender devices without per-person verification; verified trust additionally requires cross-signing identity/device verification. Outbound room keys are shared with room members' unblocked devices, including unverified devices. Readers do not need a separate verification ceremony with the bot; the selected sender-trust policy applies to commands and approvals. Plaintext rooms are deliberately unsupported in this version.

Stock Element desktop was used for live verification, mentions and thread replies. Other Matrix clients need compatible encrypted threads, mentions and verification UI; this project's current tests do not establish every client's behavior. Room membership, reactions and other metadata are not hidden by message encryption.

## ACP

The bridge is an ACP **client**. Set the absolute executable and argument array for your chosen stdio ACP **agent/server** in `[harness]`. Changing agent providers does not require a different bridge binary. The [ACP agent directory](https://agentclientprotocol.com/get-started/agents) lists available implementations; listing there does not prove compatibility with this bridge.

The current implementation requires:

- Protocol v1 initialization.
- `session/new`, text prompts, streamed agent messages and cancellation.
- When `harness.mode` is configured: that advertised mode and a successful `session/set_mode`. Omit it for adapters without ACP modes; their own permission configuration and the room tool-approval policy apply.
- `session/load` or advertised `session/resume` for contextual follow-ups. Session data must persist on the worker.
- ACP stdio MCP support for Matrix search/attachment tools.
- Agent-owned filesystem/tools. Client-provided filesystem and terminal capabilities are not advertised.

Run `doctor` before Matrix enrollment. It checks initialization, session creation, offered modes and continuation capability, and rejects a failed mode-setting request. It does not test a real follow-up, run tools, authenticate through an interactive provider flow, or prove the adapter's sandbox enforcement. Any permission request during the check is cancelled. It may leave an empty session in the agent's own storage.

**Live-tested:** Codex ACP 1.12.0 with Codex CLI 0.154.0 on Linux, using ChatGPT authentication and ACP modes `agent` and `agent-full-access`. Its [upstream instructions](https://github.com/agentclientprotocol/codex-acp) cover installation and authentication. The bridge also supports other modes the adapter advertises; `doctor` lists them. Pin adapter versions for repeatable deployments and recheck after upgrades.

**Not yet live-tested here:** ACP implementations other than Codex ACP and Grok Build. Contributions should record the adapter version, launch arguments, modes, authentication method (never credentials), new session, resumed follow-up, permission denial, and cancellation. Live checks also exercised automatic thread context, room/sender/date-filtered search, pagination, complete-thread retrieval and an encrypted image returned through MCP. On 2026-09-21, an ACP-only Codex check delivered a concurrent follow-up while a shell command was running: the original command completed, the agent incorporated the follow-up and the bridge observed successful completion. No Matrix login was used for that check. Grok live results are recorded below. Permission-choice edge cases and steering races have protocol-fixture coverage. Additional adapters and image interpretation by every model remain unverified.

## Platforms and workspaces

Linux is the deployed platform. macOS is covered by offline build/tests. The subprocess cleanup and service recipe target Unix; Windows support is not established. Rust and Nix development paths are both available; Nix is optional.

One config describes one bot and one agent/workspace profile, with one or more rooms. Threads have distinct agent sessions but **share the configured workspace and credentials**. Use a positive `max_concurrent_runs` to limit parallel work, or 0 for unlimited independent conversations. Per-thread worktrees, workspace selection and isolated concurrent sandboxes are future work.

## Prototype limits

- Full prior thread text and its parent are included. Images/files/audio/video can be retrieved through Matrix tools. No edit-as-prompt or rich artifact upload.
- Follow-ups never send cancellation. `harness.steering = "after_turn"` (default) waits for the running prompt to finish, then appends new input in the same session. `concurrent_prompt` is an explicit provider opt-in for adapters such as Codex ACP that accept another prompt while working and resolve the newest request when the steered turn completes; superseded request handlers remain alive so the SDK cannot cancel them on drop. `grok_interject` uses Grok Build's native safe-point extension and waits for any provider-owned continuation before closing the connection. Explicit `!bridge stop` still cancels.
- Room-membership mode permits membership/operator changes without resetting threads. Configured-audience mode retains strict policy/roster bindings. Worker runtime changes still need a fresh binding.
- Provider-specific model/config controls and ACP extensions are not exposed in Matrix yet. Configure defaults in the agent's own profile.
- Sync continuity/decryption failures stop progress conservatively. Backfill is limited to 500 events; longer gaps need operator recovery tooling.
- Enrollment and verification are CLI flows. Packaged binaries, a setup UI and broader provider/client testing are follow-up work.

## Grok Build

See [the Grok example](../config/grok.toml). Install the official CLI, run `grok login` as the worker user, and use `grok agent stdio`. The bridge supplies its Matrix search/thread/media MCP server through ACP, just as for other agents. No Bitwarden or SecretSpec dependency is required.

Grok Build 1.0.40 supports session load/resume but does not advertise ACP modes. Omit `harness.mode`; an explicitly requested but unavailable mode still fails preflight. Set `steering = "grok_interject"` for `_x.ai/interject`. The extension accepts text while a tool runs. A follow-up arriving after the final safe point can become a Grok-owned next turn; the bridge keeps receiving output and permission requests until the session is idle. This is not a Matrix job queue.

Use a new test thread after changing harness/provider configuration. Sessions belong to their original provider; the bridge does not pretend a Codex session can be resumed by Grok. Save the previous configuration to switch back.

**Live validation (2026-09-21):** official Grok CLI 1.0.40 on Linux, authenticated through the user's Grok subscription. A fresh encrypted mention started a shell command; a threaded follow-up was sent after the command started and before it finished. The original command completed normally and the reply incorporated the follow-up. A later message loaded the same Grok session, used a Matrix thread tool, and recalled the original test phrase. The source reference was current public `main` at `4247f661` (public snapshots can lag published CLI releases).

**Explicit-send validation (2026-09-21):** Grok CLI 1.0.40 used `send_message_to_thread` before and after a harmless shell command, then loaded the same session and sent a follow-up without repeated session instructions. A third turn asked for no reply and produced no Matrix messages. The three turns produced 2, 1 and 0 explicit sends respectively; receipts matched encrypted events authored by the bot. Ordinary ACP text was never forwarded. This used a fresh bot-authored test thread and local test inputs, not a human account or replayed user messages. Other providers have not yet been live-tested with explicit sending.
