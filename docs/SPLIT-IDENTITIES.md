# Optional split-identity service

This layout gives Matrix tokens and the crypto store to `matrix-acp-bridge`, and the agent's workspace/provider credentials to `matrix-acp-agent`. Both have private home directories. The bridge can invoke one fixed, root-owned launcher as the agent user. Neither account receives general sudo, a Docker socket, or Matrix administration credentials.

Use a dedicated systemd Linux worker. Different credentials or trust groups should use separate workers. These instructions assume an operator can use sudo on that worker; the bot does not need that access. No VPN or inbound port is required for the bot.

This is an advanced optional layout. The normal [Linux service](LINUX-SERVICE.md) runs under your existing worker account. For Matrix tools with separate users, configure `tools_socket` in a shared directory accessible to both users and grant the agent group access to that socket. The default private state socket cannot be reached across these identities.

## Install

Build the binary as described in the README. Install your chosen ACP adapter and runtime in a location available to the service user. Write a short launcher with **an absolute executable path** and its arguments. For example, if you installed `codex-acp` globally under `/usr/local/bin`:

```sh
cat > agent-launcher.sh <<'LAUNCHER'
#!/bin/sh
set -eu
export CODEX_HOME="$HOME/.codex"
export NO_BROWSER=1
export INITIAL_AGENT_MODE=agent
exec /usr/local/bin/codex-acp
LAUNCHER
sudo bash ops/install-linux.sh "$PWD/target/release/matrix-acp-bridge" "$PWD/agent-launcher.sh"
```

Replace the launcher for another ACP agent. Add any required runtime directories to PATH there. Do not embed secrets in this root-owned but world-readable script. Use the adapter's private credential files under the agent's HOME, or your chosen secret provider. The bridge itself has no secret-manager dependency.

The installer creates the two users, copies the executable/launcher, validates one narrow sudoers rule, and installs a systemd unit. It **does not** change network settings, create Matrix accounts, authenticate an agent, or start the service. Re-running it replaces those installed files; stop a running service before upgrading. Review the script before using sudo.

## Authenticate and configure

Complete the adapter's normal authentication under `matrix-acp-agent`, with HOME set to `/var/lib/matrix-acp-agent`. Use the adapter's documented login command via `sudo -u matrix-acp-agent -H …`. An interactive shell is available to the administrator with `sudo -u matrix-acp-agent -H /bin/bash`; it does not grant the agent sudo.

```sh
sudo /usr/local/bin/matrix-acp-bridge init --service-layout --config /etc/matrix-acp-bridge/config.toml
sudo chown root:matrix-acp-bridge /etc/matrix-acp-bridge/config.toml
sudo chmod 0640 /etc/matrix-acp-bridge/config.toml
sudo -u matrix-acp-bridge /usr/local/bin/matrix-acp-bridge doctor --config /etc/matrix-acp-bridge/config.toml
sudo -u matrix-acp-bridge /usr/local/bin/matrix-acp-bridge enroll --config /etc/matrix-acp-bridge/config.toml
sudo -u matrix-acp-bridge /usr/local/bin/matrix-acp-bridge verify --config /etc/matrix-acp-bridge/config.toml '@you:example.org'
```

`init --service-layout` supplies the fixed launcher, workspace, state and environment paths. You enter only Matrix IDs/URL and the adapter's mode. For Codex ACP, `agent` is the mode used in the live interoperability test. Choose your mode deliberately: it controls what the adapter may do without an approval request.

After the doctor and verification pass:

```sh
sudo systemctl enable --now matrix-acp-bridge
sudo systemctl status matrix-acp-bridge --no-pager
sudo journalctl -u matrix-acp-bridge -n 50 --no-pager
```

Run diagnostics or additional verification with the service stopped; the state directory has an exclusive lock. Start it again afterwards. In a generic script, use the long service name `matrix-acp-bridge.service` if needed.

## Operational notes

- Install Git/repository access and project secrets **for the agent identity**, scoped to its work. Provider login does not grant repository access. The bridge never copies your administrator credentials.
- The service permits writes only beneath the two service homes and its private temporary directory. Keep repositories in `/var/lib/matrix-acp-agent/workspace` or deliberately update the unit/config for a different location.
- The unit intentionally allows the fixed sudo user switch; `NoNewPrivileges=true` would prevent it. The narrow sudoers entry is for the exact launcher with **no arguments**.
- Review service logs locally before sharing. Never attach state databases, Matrix session JSON, store keys or credential files to an issue.
- Back up the complete bridge state and the agent's session/workspace state to access-controlled storage. Stop the service for a consistent initial backup and test a restore on an isolated worker. Do not run two copies with the same Matrix device/store.
- This installer is a Linux convenience, not a requirement. Containers or other supervisors may implement the same separation without sudo. The service recipe needs broader distribution testing; live interoperability has been exercised on one Ubuntu worker.
