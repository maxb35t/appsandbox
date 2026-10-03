# Ubuntu agent VM (maxb35t fork)

One long-running Ubuntu VM in App Sandbox. Several agents run inside it at the same time,
each in its own **slot**, kept apart by Anthropic's
[sandbox-runtime](https://github.com/anthropic-experimental/sandbox-runtime). The VM reaches the
web only through App Sandbox's Proxied network mode.

```
agent process (slot N, user agentN)
  └─ sandbox-runtime: own network namespace, writes only /srv/agents/N,
     can't see other slots or the shared /tmp
       └─ its proxy (allows every host, logs each one to /srv/agents/N/logs/network.log)
            └─ parent proxy 127.0.0.1:3128 = asb-relay (asb-proxy guest, vsock port 8)
                 └─ host AppSandboxProxy: LAN/private blocked after DNS, per-VM rules,
                    Proxy activity
```

**Why the "allow every host" callback.** sandbox-runtime's config has no allow-all entry:
`allowedDomains` rejects `*`, and even `*.com`. Its library takes a callback for hosts that
aren't listed, so `agent-run` passes one that allows and logs each host. Filtering happens on
the host proxy. sandbox-runtime still provides the network namespace, the filesystem rules and
the per-slot log.

**Why AppArmor's bubblewrap profile is disabled.** sandbox-runtime needs user namespaces that
carry capabilities: bubblewrap creates one, and srt's `apply-seccomp` creates a nested one to
get `CAP_SYS_ADMIN` for its PID and mount namespaces. Ubuntu blocks this in two ways. The
sysctl `kernel.apparmor_restrict_unprivileged_userns` covers unconfined programs. The AppArmor
profile `bwrap-userns-restrict` runs everything bubblewrap starts under `unpriv_bwrap`, which
denies every capability (seen as `apply-seccomp: write /proc/self/setgroups ... Permission
denied`). `setup.sh` sets the sysctl to 0 and disables that profile. The cost is more kernel
attack surface inside the VM; the VM is the boundary for that, and srt still keeps the slots
apart.

## Files

| File | What it is |
|---|---|
| `setup.sh` | Runs as root in the VM. `install [REF] [SLOTS]` installs the packages, Node.js, sandbox-runtime (pinned), Rust in `/opt/rust`, the Claude Code CLI, the relay (asb-proxy built from this repo), the slots and `agent-run`. `proxied` points apt, git, curl and npm at the relay and boots the VM to the console. `slots N` adds slots. `userns` applies only the user-namespace settings above (also part of `install`). |
| `asb-relay.service` | systemd unit for `asb-proxy guest 127.0.0.1:3128 --port 8`. |
| `agent-run`, `agent-run.mjs` | `sudo agent-run --slot N [--env-file FILE] -- COMMAND...` runs a command in slot N's sandbox as user `agentN`. `--env-file` adds `KEY=VALUE` lines (such as a job token) to the command's environment, then deletes the file, so the value is never on a command line. |
| `ubuntu_env.py` | Host side, using `asb.py`. Subcommands: `sudo` (one-time passwordless sudo for the admin user), `install`, `proxied`, `userns`, `test` (the isolation checks). |

## Building the VM

1. In App Sandbox: New Sandbox → Linux → Ubuntu 26.04 Desktop ISO. Turn SSH on and deploy the key. Set the network to **NAT for setup**.
2. Once it's online: `python ubuntu_env.py sudo Ubuntu`, then `python ubuntu_env.py install Ubuntu 4`.
3. `python ubuntu_env.py proxied Ubuntu`. Shut it down, set its network to **Proxied**, and start it.
4. `python ubuntu_env.py test Ubuntu` should print ALL PASS.
5. Take a snapshot.

## Using it from the overseer

```python
c.put("Ubuntu", r"C:\jobs\42\token.env", "jobs/42.env")
code, out, err = c.run("Ubuntu", "sudo -n agent-run --slot 3 --env-file ~/jobs/42.env -- "
                                 "claude -p 'fix the failing test'")
```
