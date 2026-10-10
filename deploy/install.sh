#!/bin/sh
# Install ai-classification-milter as a hardened systemd service (Debian/Ubuntu).
#
#   sudo deploy/install.sh                       # uses dist/ or target/release/ binary
#   sudo deploy/install.sh --binary ./milter      # explicit binary
#   sudo deploy/install.sh --binary ./milter --start
#
# Idempotent: safe to re-run after each deploy (binary + unit only).
# Config files are NEVER overwritten once they exist.
set -eu

SERVICE=ai-classification-milter
RUN_USER=_ai-milter
PREFIX=/usr/local/sbin
UNIT=/etc/systemd/system/${SERVICE}.service
ENV_FILE=/etc/ai-classification-milter.env
ENV_EXAMPLE=/etc/ai-classification-milter.env.example
TOML=/etc/ai-classification-milter.toml
STATE_DIR=/var/lib/${SERVICE}
REPO_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BINARY=""

die() { echo "error: $*" >&2; exit 1; }
say() { printf '==> %s\n' "$*"; }

[ "$(id -u)" = "0" ] || die "run as root (sudo)"

while [ $# -gt 0 ]; do
  case "$1" in
    --binary) [ $# -ge 2 ] || die "--binary needs a path"; BINARY=$2; shift 2 ;;
    --start)  START=1; shift ;;
    -h|--help) sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

# --- 1. service user (no login, no home) --------------------------------------
if ! id -u "$RUN_USER" >/dev/null 2>&1; then
  say "creating system user $RUN_USER"
  useradd --system --no-create-home --home-dir /nonexistent \
          --shell /usr/sbin/nologin "$RUN_USER"
else
  say "service user $RUN_USER already exists"
fi

# --- 2. binary ----------------------------------------------------------------
if [ -z "$BINARY" ]; then
  for cand in "$REPO_DIR/dist/${SERVICE}-linux-amd64" \
              "$REPO_DIR/target/x86_64-unknown-linux-musl/release/${SERVICE}" \
              "$REPO_DIR/target/release/${SERVICE}"; do
    [ -x "$cand" ] && BINARY=$cand && break
  done
fi
[ -n "$BINARY" ] || die "no binary found; pass --binary <path> or build one (just build-linux-amd64)"
[ -f "$BINARY" ] || die "binary not found: $BINARY"

say "installing $BINARY -> $PREFIX/$SERVICE"
install -m 0755 "$BINARY" "$PREFIX/$SERVICE"

# --- 3. config (never clobber) ------------------------------------------------
if [ -f "$TOML" ]; then
  say "keeping existing $TOML"
else
  say "installing $TOML from example (review it!)"
  install -m 0640 -o root -g "$RUN_USER" \
    "$REPO_DIR/${SERVICE}.toml.example" "$TOML"
fi

if [ -f "$ENV_FILE" ]; then
  say "keeping existing $ENV_FILE (set OPENROUTER_API_KEY in it)"
else
  say "installing $ENV_FILE from example — EDIT IT BEFORE STARTING"
  install -m 0640 -o root -g "$RUN_USER" \
    "$REPO_DIR/deploy/${SERVICE}.env.example" "$ENV_FILE"
fi
chown root:"$RUN_USER" "$ENV_FILE"
chmod 0640 "$ENV_FILE"

# --- 4. state dir (StateDirectory= does this too; explicit for clarity) -------
install -d -m 0750 -o "$RUN_USER" -g "$RUN_USER" "$STATE_DIR"

# --- 5. unit ------------------------------------------------------------------
say "installing $UNIT"
install -m 0644 "$REPO_DIR/deploy/${SERVICE}.service" "$UNIT"
systemctl daemon-reload
systemctl enable "$SERVICE" >/dev/null 2>&1 || true

# --- 6. verification ----------------------------------------------------------
if [ -n "${START:-}" ]; then
  say "starting $SERVICE"
  systemctl restart "$SERVICE"
  sleep 1
  if systemctl is-active --quiet "$SERVICE"; then
    say "service is active"
  else
    echo "service failed to start — diagnostics:" >&2
    systemctl status "$SERVICE" --no-pager || true
    journalctl -u "$SERVICE" -n 30 --no-pager || true
    exit 1
  fi
  systemctl reset-failed "$SERVICE" 2>/dev/null || true
fi

cat <<EOF

Next steps:
  1. Edit the secret and paths:
       vi $ENV_FILE          # OPENROUTER_API_KEY=...
       vi $TOML              # spam_dir / eval_log / models / [dashboard]
  2. Start:
       systemctl restart $SERVICE
       systemctl status  $SERVICE --no-pager
       journalctl -u $SERVICE -f
  3. Wire up Postfix (main.cf):
       smtpd_milters = { inet:127.0.0.1:8891, connect_timeout=10s, content_timeout=30s, default_action=accept }
       milter_protocol = 6
       milter_default_action = accept
       postfix reload
  4. Smoke test:
       sendmail -v someone@example.com < some.eml && tail -50 /var/log/mail.log
       # look for X-LLM-Verdict in the delivered headers
EOF