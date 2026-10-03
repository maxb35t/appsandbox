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
| `setup.sh` | Runs as root in the VM. `install [REF] [SLOTS]` installs the packages, Node.js, sandbox-runtime (pinned), Rust in `/opt/rust`, the Claude Code CLI, the relay (asb-proxy built from this repo), the slots and `agent-run`. `proxied` points apt and the proxy environment variables (used by git, curl, npm and cargo) at the relay, and boots the VM to the console. git and npm keep no proxy in their own config, which would bypass sandbox-runtime's authenticated proxy inside a slot. `slots N` adds slots. `userns` applies only the user-namespace settings above (also part of `install`). |
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

## Engine agent jobs: `agent-job` (engine ADR 0020)

`sudo agent-job start --slot N --dir DIR` runs one engine agent job in slot N. It returns at once.

`DIR` holds three files:
- `job.json`: exactly `agent`, `claude_version`, `effort`, `job_id`, `model`, `timeout_s` and `workdir`;
- `prompt.txt`;
- `cred.env`: exactly `CLAUDE_CODE_OAUTH_TOKEN` and `GH_TOKEN`. It is deleted at start, whatever happens.

The runner applies the same rules as the engine's Windows launcher (`ci/host/engine-agent/launcher.ps1`). In addition:
- **It runs as systemd unit `agent-job-<job_id>`,** so stopping the unit kills everything the job started (its cgroup).
- **Claude Code pin:** the job is refused unless `claude --version` is exactly `claude_version`.
- **Slot wipe:** the slot is wiped (every `agentN` process killed, its home and its temp files removed) before and after the job, so nothing passes between jobs.
- **Credentials:** they reach the job through `agent-run --env-file`, with a git credential helper that reads `GH_TOKEN` from the environment.
- **Records:** they live in `/var/lib/agent-jobs/<job_id>/`, readable by root only, so the job can't forge them. They hold `status.json`, `result.json` (Claude's JSON output, at most 4 MiB), `stderr.txt` and the slot's `network.log`.

`agent-job status|result|stop JOB_ID` read the records (bounded) or stop the job.

`setup.sh jobs` installs all of this, the GitHub CLI, Claude Code at exactly `CLAUDE_VERSION` (default 2.1.284, the engine's pin) with auto-update off, and the Rust toolchains in `RUST_TOOLCHAINS` (default `stable 1.98.1`; slots can't install toolchains). `install` runs it too.

`ubuntu_env.py jobtest VM [SLOT]` runs a real job with the agent's tokens and checks it:
- the job clones the engine repo and `claude -p` answers;
- the records are complete;
- the slot is wiped, and no credential file or process is left.
