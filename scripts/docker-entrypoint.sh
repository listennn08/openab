#!/usr/bin/env bash
# openab docker entrypoint — starts sshd (remote shell access), then execs CMD.
#
# SSH is always on. Key auth works under any user; password auth requires the
# container to run as root (sshd needs /etc/shadow access).
#
# Env vars:
#   OPENAB_SSH_PORT                  listen port (default: 2222)
#   OPENAB_SSH_AUTHORIZED_KEYS       newline-separated public keys
#   OPENAB_SSH_AUTHORIZED_KEYS_FILE  file containing public keys
#   OPENAB_SSH_PASSWORD              password for the agent user (root only)
#   OPENAB_SSH_PASSWORD_FILE         file containing the password (root only)
#   OPENAB_SSH_DISABLE               set to "true" to skip sshd entirely
set -euo pipefail

if [ "${OPENAB_SSH_DISABLE:-}" != "true" ]; then

  SSH_PORT="${OPENAB_SSH_PORT:-2222}"

  # Writable dir for host keys / authorized_keys: prefer $HOME/.ssh (persisted
  # on the PVC in k8s), fall back to /tmp (emptyDir / container layer).
  SSH_DIR="${HOME:-/home/agent}/.ssh"
  if ! { mkdir -p "$SSH_DIR" 2>/dev/null && [ -w "$SSH_DIR" ]; }; then
    SSH_DIR="/tmp/ssh"
    mkdir -p "$SSH_DIR"
  fi
  chmod 700 "$SSH_DIR" 2>/dev/null || true

  # Host keys — generated once, persisted when SSH_DIR is on the PVC.
  # Regenerate when unreadable (e.g. left behind by a previous --user root run).
  for kt in ed25519 ecdsa rsa; do
    kf="$SSH_DIR/ssh_host_${kt}_key"
    if [ ! -r "$kf" ]; then
      rm -f "$kf" 2>/dev/null || true
      ssh-keygen -q -t "$kt" -f "$kf" -N "" || true
    fi
  done

  # authorized_keys — merge: existing file (PVC) + env var + mounted file.
  # Written via tmp+mv so a foreign-owned file can be replaced in a writable dir.
  KEYS_FILE="$SSH_DIR/authorized_keys"
  KEYS_TMP="$(mktemp)"
  {
    if [ -r "$KEYS_FILE" ]; then cat "$KEYS_FILE"; fi
    if [ -n "${OPENAB_SSH_AUTHORIZED_KEYS:-}" ]; then printf '%s\n' "$OPENAB_SSH_AUTHORIZED_KEYS"; fi
    if [ -n "${OPENAB_SSH_AUTHORIZED_KEYS_FILE:-}" ] && [ -r "$OPENAB_SSH_AUTHORIZED_KEYS_FILE" ]; then
      cat "$OPENAB_SSH_AUTHORIZED_KEYS_FILE"
    fi
  } | sed 's/[[:space:]]*$//' | sed '/^$/d' | sort -u > "$KEYS_TMP"
  if [ -s "$KEYS_TMP" ]; then
    chmod 600 "$KEYS_TMP"
    mv -f "$KEYS_TMP" "$KEYS_FILE"
  else
    rm -f "$KEYS_TMP"
  fi

  # Password auth — only possible when running as root (shadow access).
  PASSWORD_AUTH=no
  SSH_PASSWORD="${OPENAB_SSH_PASSWORD:-}"
  if [ -z "$SSH_PASSWORD" ] && [ -n "${OPENAB_SSH_PASSWORD_FILE:-}" ] && [ -r "$OPENAB_SSH_PASSWORD_FILE" ]; then
    SSH_PASSWORD="$(cat "$OPENAB_SSH_PASSWORD_FILE")"
  fi
  if [ -n "$SSH_PASSWORD" ]; then
    if [ "$(id -u)" = "0" ]; then
      echo "agent:${SSH_PASSWORD}" | chpasswd
      PASSWORD_AUTH=yes
    else
      echo "[openab] WARN: SSH password ignored — password auth requires running the container as root (--user root). Key auth is active." >&2
    fi
  fi

  # PATH for non-interactive SSH commands (interactive shells get /etc/profile.d).
  ENV_TMP="$(mktemp)"
  printf 'PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin\n' > "$ENV_TMP"
  chmod 600 "$ENV_TMP"
  mv -f "$ENV_TMP" "$SSH_DIR/environment"

  SSHD_TMP="$(mktemp)"
  cat > "$SSHD_TMP" <<EOF
Port ${SSH_PORT}
ListenAddress 0.0.0.0
ListenAddress ::
HostKey ${SSH_DIR}/ssh_host_ed25519_key
HostKey ${SSH_DIR}/ssh_host_ecdsa_key
HostKey ${SSH_DIR}/ssh_host_rsa_key
PidFile ${SSH_DIR}/sshd.pid
AuthorizedKeysFile ${KEYS_FILE}
PermitUserEnvironment PATH
PermitRootLogin no
PubkeyAuthentication yes
PasswordAuthentication ${PASSWORD_AUTH}
KbdInteractiveAuthentication no
UsePAM no
AllowUsers agent
StrictModes no
MaxAuthTries 6
LoginGraceTime 30
X11Forwarding no
AllowTcpForwarding yes
AllowAgentForwarding yes
PermitTunnel no
PrintMotd no
AcceptEnv LANG LC_*
Subsystem sftp internal-sftp
EOF
  mv -f "$SSHD_TMP" "$SSH_DIR/sshd_config"

  if ! /usr/sbin/sshd -f "$SSH_DIR/sshd_config" -E "$SSH_DIR/sshd.log"; then
    echo "[openab] WARN: sshd failed to start — see $SSH_DIR/sshd.log" >&2
    [ -r "$SSH_DIR/sshd.log" ] && tail -n 20 "$SSH_DIR/sshd.log" >&2 || true
  fi
fi

exec "$@"
