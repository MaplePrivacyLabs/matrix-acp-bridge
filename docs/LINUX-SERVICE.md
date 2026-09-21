# Linux service

Run the bridge under the normal account on your dedicated agent VM. It uses that account's agent login, files and tools. No extra Linux users, sudo launcher or VPN are required. The VM and its credentials determine the agent's access.

## Configure as the worker user

Follow the README to build, authenticate your ACP agent, run `init`, `doctor`, and `enroll`. Choose absolute state/workspace paths under your existing home. Keep configuration and state private. Configure `tool_approval = "automatic"` if all tools available on this worker should run without chat approvals. Use `max_concurrent_runs = 0` to let separate threads work concurrently; they still share files and credentials.

Then install the binary and a systemd unit. Replace `worker`, `/home/worker` and the configuration path with your actual account and absolute paths:

```sh
sudo install -m 0755 target/release/matrix-acp-bridge /usr/local/bin/matrix-acp-bridge
sudoedit /etc/systemd/system/matrix-acp-bridge.service
```

```ini
[Unit]
Description=Matrix ACP bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=worker
Group=worker
WorkingDirectory=/home/worker
Environment=HOME=/home/worker
UMask=0077
ExecStart=/usr/local/bin/matrix-acp-bridge run --config /home/worker/matrix-acp/config.toml
Restart=on-failure
RestartSec=20
KillMode=control-group

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now matrix-acp-bridge
sudo systemctl status matrix-acp-bridge --no-pager
```

The adapter receives the explicit environment in `[harness.env]`. Add provider-specific variables and desktop-session variables there if its tools need them. The service does not grant extra privileges or hide the worker's existing files.

## Operation

- The live bridge creates a private local socket for Matrix tools and supplies its stdio proxy to ACP sessions automatically. No inbound TCP port is opened. Keep bridge and agent under the same account for the default socket path.
- Stop the service before enrollment, verification or store diagnostics that require exclusive state ownership. Restart afterwards. Ordinary Matrix MCP tools use the running bridge's client and do not require stopping it.
- Stop and back up the complete state, config and binary before upgrading. Preserve the Matrix device store and agent session files. Never run two workers against the same device store.
- Review logs before sharing them. Do not attach credentials, journal databases or state directories to public issues.

An [optional split-identity recipe](SPLIT-IDENTITIES.md) remains available for operators who explicitly want two service accounts. `ops/install-linux.sh` implements that advanced layout; it is not needed for the normal setup above.
