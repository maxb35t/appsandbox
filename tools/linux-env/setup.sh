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
#   setup.sh userns                  only the user-namespace settings srt needs (part of install).
#   setup.sh jobs [REF]              only the engine job pieces (part of install): the GitHub CLI,
#                                    Claude Code at exactly $CLAUDE_VERSION, the Rust toolchains in
#                                    $RUST_TOOLCHAINS, and agent-job (engine ADR 0020). Re-run it to
#                                    change the pinned versions on an existing VM.
set -euo pipefail

REPO=https://github.com/maxb35t/appsandbox
RELAY=127.0.0.1:3128
SRT_VERSION=0.0.78
# Engine ADR 0020: jobs refuse to start unless Claude Code is the version the driver names, so the VM
# carries exactly that version (the engine's pin), with auto-update off. A change is a new base snapshot.
CLAUDE_VERSION=${CLAUDE_VERSION:-2.1.284}
# Slots can't install toolchains (/opt/rust is read-only to them), so the engine's pinned toolchain
# (rust-toolchain.toml) must already be here.
RUST_TOOLCHAINS=${RUST_TOOLCHAINS:-"stable 1.98.1"}
JOB_REPO=${JOB_REPO:-https://github.com/maxb35t/engine}
LIB=/usr/local/lib/agent-run

log() { echo "== $*"; }

need_root() { [ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 1; }; }

# The fork at REF (a branch, tag or full commit ID; `git clone --branch` takes only the first two).
fetch_src() {
    local ref=$1 dir=$2
    git init -q "$dir"
    git -C "$dir" fetch -q --depth 1 "$REPO" "$ref"
    git -C "$dir" checkout -q FETCH_HEAD
}

# git and npm take the proxy from the environment (/etc/environment and profile.d here; inside a slot,
# sandbox-runtime's own authenticated proxy). A proxy in their config files would win over the
# environment and send slot traffic to sandbox-runtime's proxy without its login (407).
tool_proxy_from_env() {
    git config --system --unset-all http.proxy 2>/dev/null || true
    npm config delete --global proxy 2>/dev/null || true
    npm config delete --global https-proxy 2>/dev/null || true
}

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

# sandbox-runtime needs user namespaces that carry capabilities: bwrap makes one, and
# srt's apply-seccomp makes a nested one to get CAP_SYS_ADMIN for its PID/mount unshare.
# Ubuntu blocks both: the sysctl below covers unconfined programs, and the AppArmor
# profile bwrap-userns-restrict confines everything bwrap starts to unpriv_bwrap, which
# denies all capabilities. That profile is unloaded and disabled here (user's decision:
# the VM only runs agents and is the kernel boundary; srt still isolates the slots).
userns_setup() {
    log "sandbox-runtime needs capability-bearing user namespaces (Ubuntu restricts them)"
    echo 'kernel.apparmor_restrict_unprivileged_userns = 0' > /etc/sysctl.d/60-agent-sandbox.conf
    sysctl -q --system
    local prof=/etc/apparmor.d/bwrap-userns-restrict
    if [ -f "$prof" ]; then
        mkdir -p /etc/apparmor.d/disable
        ln -sf "$prof" /etc/apparmor.d/disable/bwrap-userns-restrict
        if grep -qw unpriv_bwrap /sys/kernel/security/apparmor/profiles 2>/dev/null; then
            apparmor_parser -R "$prof"
        fi
        log "AppArmor profile bwrap-userns-restrict disabled"
    fi
}

# Engine agent jobs (ADR 0020): pinned Claude Code and Rust toolchains, the GitHub CLI and agent-job.
cmd_jobs() {
    local ref=${1:-main} src have t
    export DEBIAN_FRONTEND=noninteractive
    tool_proxy_from_env
    log "GitHub CLI"
    command -v gh >/dev/null || { apt-get update -q; apt-get install -y -q gh; }

    log "Claude Code $CLAUDE_VERSION (exact; npm checks the package against the registry's integrity hash)"
    npm install -g -q "@anthropic-ai/claude-code@$CLAUDE_VERSION"
    have=$(DISABLE_AUTOUPDATER=1 claude --version | awk '{print $1}')
    [ "$have" = "$CLAUDE_VERSION" ] || { echo "Claude Code is $have, wanted $CLAUDE_VERSION" >&2; exit 1; }
    grep -q '^DISABLE_AUTOUPDATER=' /etc/environment || echo 'DISABLE_AUTOUPDATER=1' >> /etc/environment

    log "Rust toolchains: $RUST_TOOLCHAINS"
    for t in $RUST_TOOLCHAINS; do
        RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo PATH=/opt/rust/cargo/bin:$PATH \
            rustup toolchain install "$t" --profile default -c rustfmt -c clippy
    done
    chmod -R a+rX /opt/rust

    log "agent-job (from $REPO at $ref)"
    src=$(mktemp -d)
    fetch_src "$ref" "$src/appsandbox"
    install -d "$LIB"
    install -m 644 "$src/appsandbox/tools/linux-env/agent-job.mjs" "$LIB/agent-job.mjs"
    rm -rf "$src"
    cat > /usr/local/bin/agent-job <<'P'
#!/bin/sh
# Engine agent jobs in slots (engine ADR 0020). Run with sudo; see agent-job.mjs.
exec node /usr/local/lib/agent-run/agent-job.mjs "$@"
P
    chmod 755 /usr/local/bin/agent-job
    install -d -m 700 /var/lib/agent-jobs /etc/agent-job
    # The same rules as the engine's launcher config (ci/host/engine-agent/launcher-config.json).
    cat > /etc/agent-job/config.json <<P
{"repo":"$JOB_REPO","models":["opus","sonnet"],"efforts":["low","medium","high","xhigh","max"],"agents":["none"],"max_timeout_s":10800}
P
    chmod 600 /etc/agent-job/config.json
    log "jobs ready: Claude Code $CLAUDE_VERSION, toolchains $RUST_TOOLCHAINS, repo $JOB_REPO"
}

cmd_install() {
    local ref=${1:-main} slots=${2:-4} src
    export DEBIAN_FRONTEND=noninteractive

    log "packages"
    apt-get update -q
    apt-get install -y -q bubblewrap socat ripgrep git build-essential pkg-config curl ca-certificates \
        jq nodejs npm openssh-server gh

    userns_setup

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
    fetch_src "$ref" "$src/appsandbox"
    RUSTUP_HOME=/opt/rust/rustup CARGO_HOME=/opt/rust/cargo PATH=/opt/rust/cargo/bin:$PATH \
        cargo build -q --release --manifest-path "$src/appsandbox/tools/asb-proxy/Cargo.toml"
    install -m 755 "$src/appsandbox/tools/asb-proxy/target/release/asb-proxy" /usr/local/bin/asb-proxy
    install -m 644 "$src/appsandbox/tools/linux-env/asb-relay.service" /etc/systemd/system/asb-relay.service
    systemctl daemon-reload
    systemctl enable -q --now asb-relay.service

    log "agent-run (sandbox-runtime $SRT_VERSION)"
    install -d "$LIB"
    install -m 644 "$src/appsandbox/tools/linux-env/agent-run.mjs" "$LIB/agent-run.mjs"
    ( cd "$LIB"
      [ -f package.json ] || echo '{"name":"agent-run","private":true,"type":"module"}' > package.json
      npm install -q --no-audit --no-fund "@anthropic-ai/sandbox-runtime@$SRT_VERSION" )
    install -m 755 "$src/appsandbox/tools/linux-env/agent-run" /usr/local/bin/agent-run
    rm -rf "$src"

    make_slots "$slots"
    cmd_jobs "$ref"
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
    tool_proxy_from_env
    log "boot to the console (the desktop can be switched back on: systemctl set-default graphical.target)"
    systemctl set-default multi-user.target
    log "done. Set the VM's network to Proxied (4) in App Sandbox and restart it."
}

need_root
case "${1:-}" in
    install) shift; cmd_install "$@" ;;
    proxied) cmd_proxied ;;
    slots)   make_slots "${2:?count}" ;;
    userns)  userns_setup ;;
    jobs)    shift; cmd_jobs "$@" ;;
    *) sed -n '2,17p' "$0"; exit 2 ;;
esac
