# Compatibility and current limits

## Matrix

The bridge is an ordinary client of the Matrix Client-Server API, built with Matrix Rust SDK **0.19.1**. The bot can live on a hosted or self-hosted server; no server admin API, custom DNS, VPN, federation restriction, or Element fork is required. Accounts and rooms can span servers, subject to those servers' normal policies.

Enrollment currently uses Matrix password login and cross-signing bootstrap. A server with only browser-based OIDC/SSO login needs additional enrollment support. There is no generic account-creation API: create the bot using your provider's usual signup/invite process. Use a dedicated account rather than repurposing an existing personal account with other sessions/keys.

Work rooms must be encrypted. Senders must be allowlisted operators, current members, and verified through their cross-signing identity/device. Outbound room keys are shared with room members' unblocked devices, including unverified devices. Readers do not need a separate verification ceremony with the bot; the stricter verified-sender check applies to commands and approvals. Plaintext rooms are deliberately unsupported in this version.

Stock Element desktop was used for live verification, mentions and thread replies. Other Matrix clients need compatible encrypted threads, mentions and verification UI; this project's current tests do not establish every client's behavior. Room membership, reactions and other metadata are not hidden by message encryption.

## ACP

The bridge is an ACP **client**. Set the absolute executable and argument array for your chosen stdio ACP **agent/server** in `[harness]`. Changing agent providers does not require a different bridge binary. The [ACP agent directory](https://agentclientprotocol.com/get-started/agents) lists available implementations; listing there does not prove compatibility with this bridge.

The current implementation requires:

- Protocol v1 initialization.
- `session/new`, text prompts, streamed agent messages and cancellation.
- Advertised session permission modes and a successful `session/set_mode` for your configured mode.
- `session/load` or advertised `session/resume` for contextual follow-ups. Session data must persist on the worker.
- Agent-owned filesystem/tools. Client-provided filesystem and terminal capabilities are not advertised.

Run `doctor` before Matrix enrollment. It checks initialization, session creation, offered modes and continuation capability, and rejects a failed mode-setting request. It does not test a real follow-up, run tools, authenticate through an interactive provider flow, or prove the adapter's sandbox enforcement. Any permission request during the check is cancelled. It may leave an empty session in the agent's own storage.

**Live-tested:** Codex ACP 1.12.0 with Codex CLI 0.154.0 on Linux, using ChatGPT authentication and ACP mode `agent`. Its [upstream instructions](https://github.com/agentclientprotocol/codex-acp) cover installation and authentication. The bridge also supports other modes the adapter advertises; `doctor` lists them. Pin adapter versions for repeatable deployments and recheck after upgrades.

**Not yet live-tested here:** other ACP implementations. Contributions should record the adapter version, launch arguments, modes, authentication method (never credentials), new session, resumed follow-up, permission denial, and cancellation. Approval/cancellation currently have protocol-fixture coverage; the first live test covered mention, answer and resumed follow-up.

## Platforms and workspaces

Linux is the deployed platform. macOS is covered by offline build/tests. The subprocess cleanup and service recipe target Unix; Windows support is not established. Rust and Nix development paths are both available; Nix is optional.

One config describes one bot and one agent/workspace profile, with one or more rooms. Threads have distinct agent sessions but **share the configured workspace and credentials**. Start with `max_concurrent_runs = 1` for coding work. Per-thread worktrees, workspace selection and isolated concurrent sandboxes are future work.

## Prototype limits

- Text messages only. No attachment download, image input, edit-as-prompt, room-history ingestion or rich artifact upload.
- No mid-turn queuing or steering. Wait, or cancel and resend.
- Policy/membership changes invalidate conversation bindings. Start a new thread after reviewing the change; existing room-mode bindings need future migration tooling.
- Provider-specific model/config controls and ACP extensions are not exposed in Matrix yet. Configure defaults in the agent's own profile.
- Sync continuity/decryption failures stop progress conservatively. Backfill is limited to 500 events; longer gaps need operator recovery tooling.
- Enrollment and verification are CLI flows. Packaged binaries, a setup UI and broader provider/client testing are follow-up work.
