#!/usr/bin/env bash
# Install a bridge binary and an operator-reviewed ACP launcher on a dedicated
# systemd Linux worker. Does not authenticate, enroll, enable or start anything.
set -euo pipefail
umask 077

if [[ $(id -u) != 0 || $# != 2 ]]; then
  echo "Usage: sudo $0 /absolute/bridge-binary /absolute/agent-launcher.sh" >&2
  exit 1
fi
binary=$1
backend=$2
[[ $binary = /* && -f $binary && -x $binary && $backend = /* && -f $backend ]] || {
  echo 'Provide absolute paths to a built executable and a reviewed launcher script.' >&2
  exit 1
}
for command in useradd getent install visudo systemctl; do command -v "$command" >/dev/null; done

for name in matrix-acp-bridge matrix-acp-agent; do
  if getent passwd "$name" >/dev/null; then
    entry=$(getent passwd "$name")
    IFS=: read -r _ _ uid _ _ directory _ <<< "$entry"
    [[ $uid != 0 && $directory == "/var/lib/$name" ]] || {
      echo "Existing account $name has an unexpected identity/home; refusing to change it." >&2
      exit 1
    }
  else
    useradd --system --user-group --create-home --home-dir "/var/lib/$name" --shell /usr/sbin/nologin "$name"
  fi
  install -d -o "$name" -g "$name" -m 0700 "/var/lib/$name"
done
install -d -o matrix-acp-bridge -g matrix-acp-bridge -m 0700 /var/lib/matrix-acp-bridge/state
install -d -o matrix-acp-agent -g matrix-acp-agent -m 0700 /var/lib/matrix-acp-agent/workspace
install -d -o root -g matrix-acp-bridge -m 0750 /etc/matrix-acp-bridge
install -d -o root -g root -m 0755 /usr/local/bin /usr/local/libexec
install -o root -g root -m 0755 "$binary" /usr/local/bin/matrix-acp-bridge
install -o root -g root -m 0755 "$backend" /usr/local/libexec/matrix-acp-backend

cat > /usr/local/libexec/matrix-acp-agent <<'LAUNCHER'
#!/bin/sh
set -eu
umask 077
cd /var/lib/matrix-acp-agent/workspace
exec env -i HOME=/var/lib/matrix-acp-agent PATH=/usr/local/bin:/usr/bin:/bin \
  /usr/local/libexec/matrix-acp-backend
LAUNCHER
chown root:root /usr/local/libexec/matrix-acp-agent
chmod 0755 /usr/local/libexec/matrix-acp-agent
sudoers_tmp=$(mktemp /etc/sudoers.d/.matrix-acp-bridge.XXXXXX)
trap 'rm -f "$sudoers_tmp"' EXIT
cat > "$sudoers_tmp" <<'SUDOERS'
Defaults:matrix-acp-bridge !use_pty
matrix-acp-bridge ALL=(matrix-acp-agent) NOPASSWD: /usr/local/libexec/matrix-acp-agent ""
SUDOERS
chmod 0440 "$sudoers_tmp"
visudo -cf "$sudoers_tmp" >/dev/null
install -m 0440 -o root -g root "$sudoers_tmp" /etc/sudoers.d/matrix-acp-bridge

cat > /etc/systemd/system/matrix-acp-bridge.service <<'UNIT'
[Unit]
Description=Matrix ACP bridge
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=3

[Service]
Type=simple
User=matrix-acp-bridge
Group=matrix-acp-bridge
WorkingDirectory=/
UMask=0077
ExecStart=/usr/local/bin/matrix-acp-bridge run --config /etc/matrix-acp-bridge/config.toml
Restart=on-failure
RestartSec=20
KillMode=control-group
ProtectHome=true
ProtectSystem=strict
ReadWritePaths=/var/lib/matrix-acp-bridge /var/lib/matrix-acp-agent
PrivateTmp=true

[Install]
WantedBy=multi-user.target
UNIT
chmod 0644 /etc/systemd/system/matrix-acp-bridge.service
systemctl daemon-reload
printf '%s\n' 'Installed. The service has not been enabled or started.' \
  'Next: authenticate the ACP adapter as matrix-acp-agent, then follow docs/LINUX-SERVICE.md.'
