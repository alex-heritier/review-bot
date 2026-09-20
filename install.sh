#!/usr/bin/env bash
# install.sh — set up the full review-bot stack on this machine:
#   1. check prerequisites (git >= 2.41, Rust, Node/npm)
#   2. install the OpenCodeReview engine (ocr)
#   3. build and install the review-bot binary
#   4. provision a dedicated service user + systemd unit
#   5. report what's left to do (GitHub App + wizard)
#
# Idempotent: safe to re-run after every pull. Run as root:
#   sudo ./install.sh
set -euo pipefail

APP=review-bot
BIN_NAME=github-pr-review-bot
BIN_DST=/usr/local/bin/$APP
SERVICE_USER=review-bot
SERVICE_HOME=/home/$SERVICE_USER
UNIT=/etc/systemd/system/$APP.service
REPO_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
WEBHOOK_PORT=${WEBHOOK_PORT:-3000}

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
log()  { printf '    %s\n' "$*"; }
die()  { printf '\033[1;31mError:\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root: sudo $REPO_DIR/install.sh"
[ -f "$REPO_DIR/Cargo.toml" ] || die "run me from the review-bot repository directory"

# ---------------------------------------------------------------- prerequisites
step "Checking prerequisites"
command -v git >/dev/null 2>&1 || die "git is required (>= 2.41)"
git_ver=$(git --version | awk '{print $3}')
if [ "$(printf '%s\n2.41\n' "$git_ver" | sort -V | head -n1)" != "2.41" ]; then
    die "git >= 2.41 required (ocr needs it), found $git_ver"
fi
log "git $git_ver"

command -v cargo >/dev/null 2>&1 || die "Rust is required to build the bot:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source \$HOME/.cargo/env   # then re-run this script"
log "$(rustc --version 2>/dev/null || cargo --version)"

command -v npm >/dev/null 2>&1 || die "Node.js/npm is required to install the ocr engine, e.g.:
  dnf install nodejs    # Fedora
  apt install nodejs    # Debian/Ubuntu"
log "node $(node --version)"

# --------------------------------------------------------------------- ocr
step "Installing OpenCodeReview (ocr)"
if command -v ocr >/dev/null 2>&1; then
    log "already present: $(ocr --version | head -n1)"
    log "upgrade (optional): npm install -g @alibaba-group/open-code-review@latest"
else
    npm install -g @alibaba-group/open-code-review
    log "installed $(ocr --version | head -n1)"
fi
command -v ocr >/dev/null 2>&1 || die "ocr is not on PATH after npm install"

# --------------------------------------------------------------------- build
step "Building review-bot (release)"
( cd "$REPO_DIR" && cargo build --release --locked )
[ -x "$REPO_DIR/target/release/$BIN_NAME" ] || die "build produced no binary"

step "Installing binary -> $BIN_DST"
install -Dm755 "$REPO_DIR/target/release/$BIN_NAME" "$BIN_DST"

# ------------------------------------------------------------------- service
if [ -d /run/systemd/system ]; then
    step "Provisioning $SERVICE_USER user + systemd service"
    id -u "$SERVICE_USER" >/dev/null 2>&1 || \
        useradd --system --create-home --home-dir "$SERVICE_HOME" --shell /bin/bash "$SERVICE_USER"
    log "service user: $SERVICE_USER (holds config, App key, repo+ocr cache)"

    cat > "$UNIT" <<EOF
[Unit]
Description=GitHub PR review bot (thin shell around ocr)
After=network-online.target
Wants=network-online.target

[Service]
User=$SERVICE_USER
Group=$SERVICE_USER
WorkingDirectory=$SERVICE_HOME
ExecStart=$BIN_DST
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info
# The bot writes only to its own home (.review-bot.yaml, ocr state, repo cache).
ProtectSystem=strict
ReadWritePaths=$SERVICE_HOME
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload

    if [ -f "$SERVICE_HOME/.review-bot.yaml" ]; then
        systemctl enable --now "$APP"
        log "service enabled and running: systemctl status $APP"
    else
        systemctl disable "$APP" >/dev/null 2>&1 || true
        log "unit installed; service NOT started until the wizard has run (below)"
    fi
    MANAGE_HINT="systemctl $APP"
else
    step "Skipping systemd (not present on this machine)"
    log "foreground mode: sudo -u $SERVICE_USER -H $BIN_DST   (first run starts the wizard)"
    id -u "$SERVICE_USER" >/dev/null 2>&1 || \
        useradd --system --create-home --home-dir "$SERVICE_HOME" --shell /bin/bash "$SERVICE_USER"
    MANAGE_HINT="the foreground process"
fi

# -------------------------------------------------------------------- report
cat <<EOF

$(printf '\033[1mDone.\033[0m  Remaining setup:')

  1. GitHub App (web UI, once): https://github.com/settings/apps
     - Permissions: Pull requests: Read and write · Contents: Read-only · Metadata: Read
     - Subscribe to the Pull request event; webhook URL:
         https://YOUR_DOMAIN/webhooks/github   (reverse-proxy port $WEBHOOK_PORT)
     - Generate + download a private key; note the App ID
     - Install the App on your account
  2. First run as the service user (interactive setup wizard):
         sudo -u $SERVICE_USER -H $BIN_DST
     Saves $SERVICE_HOME/.review-bot.yaml (0600) with the App ID, key path,
     webhook secret and OpenAI-compatible endpoint ocr should use.
     Point the key path at a file readable by $SERVICE_USER, e.g.
         $SERVICE_HOME/review-bot.private-key.pem
  3. Expose port $WEBHOOK_PORT over HTTPS (Caddy/nginx) and start the service:
         sudo systemctl enable --now $APP   # or manage $MANAGE_HINT
  4. Sanity check:  curl -s localhost:$WEBHOOK_PORT/health   -> ok

OpenCodeReview runs with its own config under $SERVICE_HOME/.opencodereview;
the API key only passes to it per-review through the environment.
EOF
