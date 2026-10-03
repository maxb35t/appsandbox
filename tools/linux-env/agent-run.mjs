// maxb35t fork: the sandbox half of agent-run (started by /usr/local/bin/agent-run as the
// slot's user). Wraps the command with Anthropic's sandbox-runtime library.
//
// sandbox-runtime's config has no "allow every site" entry (allowedDomains rejects "*"), so
// allowedDomains stays empty and the ask callback allows each host and logs it. Filtering
// is the host proxy's job (App Sandbox Proxied mode: LAN/private addresses blocked after DNS,
// optional per-VM allow/deny lists); traffic reaches it through parentProxy, the guest relay.
import { SandboxManager } from '@anthropic-ai/sandbox-runtime'
import { spawn } from 'node:child_process'
import fs from 'node:fs'

const slot = process.env.ASB_SLOT
const home = `/srv/agents/${slot}`
const relay = 'http://127.0.0.1:3128'

// sandbox-runtime gives the command TMPDIR=/tmp/claude, shared by every slot; use the slot's
// own temp folder instead (the shared /tmp is hidden below).
process.env.CLAUDE_CODE_TMPDIR = `${home}/tmp`

// Per-job environment from --env-file (then deleted).
const jobEnv = {}
if (process.env.ASB_JOB_ENV) {
  const f = process.env.ASB_JOB_ENV
  for (const line of fs.readFileSync(f, 'utf8').split(/\r?\n/)) {
    const m = /^\s*([A-Za-z_][A-Za-z0-9_]*)=(.*)$/.exec(line)
    if (m) jobEnv[m[1]] = m[2]
  }
  fs.rmSync(f, { force: true })
}
delete process.env.ASB_JOB_ENV

const netLog = fs.createWriteStream(`${home}/logs/network.log`, { flags: 'a' })

const config = {
  network: {
    allowedDomains: [],
    deniedDomains: [],
    parentProxy: { http: relay, https: relay },
  },
  filesystem: {
    // Other slots, admin homes and the shared temp folders are unreadable (each slot has
    // its own TMPDIR); this slot's folder stays readable.
    denyRead: ['/srv/agents', '/home', '/root', '/tmp', '/var/tmp', '/dev/shm'],
    allowRead: [home],
    allowWrite: [home],
    denyWrite: [],
  },
}

await SandboxManager.initialize(config, async ({ host, port }) => {
  netLog.write(`${new Date().toISOString()} ${host}:${port}\n`)
  return true
})

const quote = a => (/^[A-Za-z0-9_@%+=:,./-]+$/.test(a) ? a : `'${a.replace(/'/g, `'\\''`)}'`)
const command = process.argv.slice(2).map(quote).join(' ')
const wrapped = await SandboxManager.wrapWithSandbox(command)
const child = spawn(wrapped, {
  shell: true,
  stdio: 'inherit',
  env: { ...process.env, ...jobEnv },
})

const finish = code => {
  try { SandboxManager.cleanupAfterCommand() } catch {}
  netLog.end(() => process.exit(code))
}
child.on('exit', (code, signal) => finish(signal ? 128 + 15 : (code ?? 0)))
child.on('error', err => { console.error(`agent-run: ${err.message}`); finish(1) })
for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, () => child.kill(sig))
