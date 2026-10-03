# Windows agent environment 2: no desktop (maxb35t fork)

Jobs that need Windows or Windows tooling, but no GPU and no display, run in a **throwaway instance** of the Windows agent VM (`AgentTest`, snapshot `windows-agent-base-v1`).

**Each instance:**
- has the GPU off and no display window;
- uses Proxied mode, inherited from `AgentTest`: no network adapter, and the web only through the host proxy;
- runs its job over SSH in session 0.

**Each job:**
- gets a fresh instance from the snapshot;
- the instance is created with fast stop and auto-delete, so nothing survives the job and the base is never written.

**Environment 3** (GPU and desktop) is the same instance with `--gpu`, using `asb.py`'s `run_desktop` for anything that presents or needs the GPU.

No App Sandbox changes: this uses the instance options from plan 2 (`create_instance` with `gpu_mode`, `ram_mb`, `fast_stop`, `auto_delete` and `ttl_minutes`).

## Use

```
python windows_env.py run [--ram MB] [--cores N] [--gpu] [--env-file F] -- COMMAND
python windows_env.py test [--ram MB] [--gpu]
python windows_env.py measure [--ram MB] [--parallel N]
```

`--vm` and `--snap` choose another VM or snapshot (default `AgentTest` and `windows-agent-base-v1`).

**`run`:**
- runs `COMMAND` with cmd.exe in `C:\job` of a new instance, and returns its exit code;
- `--env-file`: a file of `KEY=VALUE` lines (such as a job token), copied into the instance, loaded into the job's environment and deleted before the command starts. The host copy is deleted too.

**`test` checks:**
- Proxied mode, no network adapter, no NVIDIA adapter, no display, and that jobs run in session 0;
- the web through the proxy, the LAN refused, HTTPS to an uncached host, and `git clone`;
- a crate with crates.io dependencies builds with `cargo test --release` (MSVC);
- the env-file token reaches the command;
- the instance is removed, and the base is untouched.

**`measure`:** times create-to-SSH and the build, and reads memory in use, for one or several instances at once.

## Certificate revocation

An instance has no network adapter, so Windows treats it as offline. Its certificate checker (used through Schannel by cargo, git and curl) then never fetches revocation data, even with the WinHTTP proxy set: no OCSP or CRL request reaches the proxy. So every host whose revocation status isn't already cached in the base fails with `CRYPT_E_REVOCATION_OFFLINE`.

The job wrapper sets this up before the command starts (a job's env file can override it):
- **cargo:** `CARGO_HTTP_CHECK_REVOKE=false`;
- **git:** `http.schannelCheckRevoke=false`, through `GIT_CONFIG_*`;
- **curl:** `ssl-revoke-best-effort`, through `CURL_HOME`.

Certificates and host names are still verified; only the revocation lookup is skipped.

## Measured (3 October 2026: RTX 4090 laptop, 64 GB RAM, 32 logical processors)

Each instance has 8 cores (the VM's default). The build is the `test` crate (serde, serde_json, regex) with `cargo test --release`.

| Instances × RAM | Ready for SSH | Memory idle | Memory after build | Build | Wall time |
|---|---|---|---|---|---|
| 1 × 6 GB | 8 s | 1.94 GB | 2.03 GB | 15 s | — |
| 1 × 4 GB | 8 s | 1.91 GB | 1.96 GB | 14 s | 35 s |
| 3 × 4 GB at once | 8 s each | 1.90 GB | 1.95 GB | 18 s each | 46 s |

- **Idle use is about 1.9 GB.** That includes the desktop the base still logs on to at boot. A lighter base without automatic logon wouldn't save enough to justify a base rebuild.
- **4 GB is enough for small jobs.** The default is 6 GB, because "after build" isn't the peak: larger builds, such as a whole workspace, use more while they run.
- **Running three at once** added about 4 s per build.

## Engine agent jobs: `agent-job.ps1` (engine ADR 0020)

`agent-job.ps1` is the job runner inside an instance. The driver copies it and the job's inputs into `C:\job` (`in\job.json`, `in\prompt.txt` and `in\cred.env`), then runs it:
- over SSH for `windows`;
- through `run_desktop` for `windows-desktop`.

It applies the same rules as the engine's launcher, plus three checks:
- **Claude Code pin:** the job is refused unless `claude --version` is exactly `claude_version`.
- **Credentials:** `cred.env` holds exactly the two credentials and is deleted before anything runs.
- **Revocation workaround:** as in the job wrapper above.

Records go to `C:\job\out`, and the last line of output is the final status as JSON. They are untrusted, because the job runs as the same administrator. The instance is deleted after the job, taking the credentials with it.

`windows_env.py jobtest [--gpu]` runs a real job with the agent's tokens in a fresh instance and checks it. Run it elevated.
