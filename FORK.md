# maxb35t/appsandbox

A fork of [jamesstringer90/appsandbox](https://github.com/jamesstringer90/appsandbox) with
changes for running throwaway, isolated agent VMs. Upstream treats pull requests as
suggestions, so this fork is long-lived. Its patch set is kept small and self-contained.

## Rules

- Only **host-side** code is changed (`AppSandbox.exe`, `appsandbox_core.dll`). Nothing that
  runs inside a VM is touched, so the upstream release's signed drivers, guest agent and
  resources are used as they are.
- New features are **switchable** (a setting or a per-request option). Changes that fix
  behaviour are not.
- Upstream releases come in by **merge**, never by rebase.
- Fork builds report their version as `<upstream>+mx.N` (`/v1/version`, `host.json`). The binaries'
  file versions use revision `N`.

## Installing a fork build

1. Download the `appsandbox-fork-x64-<sha>` artifact from the **fork build** workflow run.
2. Stop App Sandbox (close the window or stop the headless daemon).
3. In the upstream release folder that matches the fork's base version, back up
   `AppSandbox.exe`, `appsandbox_core.dll` and `web\app.js`. Then replace them with the
   artifact's copies (`app.js` is the GUI script; the GUI isn't needed for headless use).
4. Start App Sandbox. `/v1/version` should report `…+mx.N`.

To roll back, restore the two backed-up files.

## Changes

| Change | Status |
|---|---|
| Fork version tag and CI build | done |
| Dedicated relay channel: Hyper-V socket port 8, per VM, `relayChannel` (off by default) | done |
| Chained snapshots: snapshot the current branch; snapshots report `parent`; deleting a snapshot with children is refused (409) | done |
| Throwaway concurrent instances from a snapshot | planned |
