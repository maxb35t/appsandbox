#!/bin/bash
# maxb35t fork: sets up an App Sandbox Ubuntu VM as a long-running agent host.
# Run as root inside the VM. Idempotent: safe to run again.
#
#   setup.sh install [REF] [SLOTS]   packages, Node.js, sandbox-runtime, Rust, Claude Code,
#                                    the guest relay (asb-proxy built from the fork at REF),
#                                    agent slots and agent-run. Needs internet (NAT).
#   setup.sh proxied                 web through the relay only: proxy settings for apt, git,
#                                    curl, npm and cargo, and boot to the console. Then set
#                                    the VM's network to Proxied (4) in App Sandbox.
#   setup.sh slots N                 make sure slots 1..N exist.
set -euo pipefail

REPO=https://github.com/maxb35t/appsandbox
RELAY=127.0.0.1:3128
SRT_VERSION=0.0.78
LIB=/usr/local/lib/agent-run

log() { echo "== $*"; }

need_root() { [ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 1; }; }

make_slots() {
    local n=$1 i
    mkdir -p /srv/agents
    chmod 755 /srv/agents
    for i in $(seq 1 "$n"); do
        if ! id "agent$i" >/dev/null 2>&1; then
            useradd --home-dir "/srv/agents/$i" --create-home --shell /bin/bash \
                    --user-group --comment "agent slot $i" "agent$i"
            passwd -l "agent$i" >/dev/null
        fi
        install -d -o "agent$i" -g "agent$i" -m 700 "/srv/agents/$i"
        install -d -o "agent$i" -g "agent$i" -m 700 "/srv/agents/$i/work" "/srv/agents/$i/tmp" \
                "/srv/agents/$i/logs" "/srv/agents/$i/.cargo"
        chown -R "agent$i:agent$i" "/srv/agents/$i/logs" "/srv/agents/$i/tmp"
    done
    log "slots 1..$n ready under /srv/agents"
}

cmd_install() {
    local ref=${1:-main} slots=${2:-4} src
    export DEBIAN_FRONTEND=noninteractive

    log "packages"
    apt-get update -q
    apt-get install -y -q bubblewrap socat ripgrep git build-essential pkg-config curl ca-certificates \
        jq nodejs npm openssh-server

    log "sandbox-runtime needs capability-bearing user namespaces (Ubuntu restricts them)"
    echo 'kernel.apparmor_restrict_unprivileged_userns = 0' > /etc/sysctl.d/60-agent-sandbox.conf
    sysctl -q --system

    log "Rust (system-wide in /opt/rust; each slot keeps its own cargo cache)"
    if [ ! -x /opt/rust/cargo/bin/rustc ]; then
        mkdir -p /opt/rust
        curl -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh
        RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo \
            sh /tmp/rustup-init.sh -y --no-modify-path --profile default --default-toolchain stable
        rm -f /tmp/rustup-init.sh
    fi
    chmod -R a+rX /opt/rust
    cat > /etc/profile.d/50-rust.sh <<'P'
export RUSTUP_HOME=/opt/rust/rustup
export PATH=/opt/rust/cargo/bin:$PATH
P

    log "guest relay (asb-proxy from $REPO at $ref)"
    src=$(mktemp -d)
    git clone -q --depth 1 --branch "$ref" "$REPO" "$src/appsandbox"
    RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo PATH=/opt/rust/cargo/bin:$PATH \
        cargo build -q --release --manifest-path "$src/appsandbox/tools/asb-proxy/Cargo.toml"
    install -m 755 "$src/appsandbox/tools/asb-proxy/target/release/asb-proxy" /usr/local/bin/asb-proxy
    install -m 644 "$src/appsandbox/tools/linux-env/asb-relay.service" /etc/systemd/system/asb-relay.service
    systemctl daemon-reload
    systemctl enable -q --now asb-relay.service

    log "Claude Code CLI"
    npm install -g -q @anthropic-ai/claude-code

    log "agent-run (sandbox-runtime $SRT_VERSION)"
    install -d "$LIB"
    install -m 644 "$src/appsandbox/tools/linux-env/agent-run.mjs" "$LIB/agent-run.mjs"
    ( cd "$LIB"
      [ -f package.json ] || echo '{"name":"agent-run","private":true,"type":"module"}' > package.json
      npm install -q --no-audit --no-fund "@anthropic-ai/sandbox-runtime@$SRT_VERSION" )
    install -m 755 "$src/appsandbox/tools/linux-env/agent-run" /usr/local/bin/agent-run
    rm -rf "$src"

    make_slots "$slots"
    log "install done. Next: setup.sh proxied, then set the VM's network to Proxied."
}

cmd_proxied() {
    log "proxy settings -> $RELAY (the guest relay)"
    cat > /etc/profile.d/40-asb-proxy.sh <<P
export http_proxy=http://$RELAY https_proxy=http://$RELAY
export HTTP_PROXY=http://$RELAY HTTPS_PROXY=http://$RELAY
export no_proxy=localhost,127.0.0.1,::1 NO_PROXY=localhost,127.0.0.1,::1
P
    grep -q '^http_proxy=' /etc/environment || cat >> /etc/environment <<P
http_proxy=http://$RELAY
https_proxy=http://$RELAY
no_proxy=localhost,127.0.0.1,::1
P
    echo "Acquire::http::Proxy \"http://$RELAY\"; Acquire::https::Proxy \"http://$RELAY\";" \
        > /etc/apt/apt.conf.d/95asb-proxy
    git config --system http.proxy "http://$RELAY"
    npm config set --global proxy "http://$RELAY"
    npm config set --global https-proxy "http://$RELAY"
    log "boot to the console (the desktop can be switched back on: systemctl set-default graphical.target)"
    systemctl set-default multi-user.target
    log "done. Set the VM's network to Proxied (4) in App Sandbox and restart it."
}

need_root
case "${1:-}" in
    install) shift; cmd_install "$@" ;;
    proxied) cmd_proxied ;;
    slots)   make_slots "${2:?count}" ;;
    *) sed -n '2,13p' "$0"; exit 2 ;;
esac
