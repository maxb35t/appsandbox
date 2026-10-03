// maxb35t fork: the in-VM job runner for engine agent jobs (engine ADR 0020, Decision item 4).
// Runs as root. It applies the rules of the engine's Windows launcher (ci/host/engine-agent/launcher.ps1):
// the same job.json fields, patterns and size bounds, job ids never reused, a fresh clone in a fresh
// workdir, `claude -p` with the job's model alias and effort, a timeout, and status/result records the
// job can't forge (they live in /var/lib/agent-jobs, root only). In addition it refuses a job unless
// Claude Code is the version the job names, and it wipes the slot before and after every job.
//
//   agent-job start --slot N --dir DIR   DIR holds job.json, prompt.txt and cred.env (deleted here)
//   agent-job status JOB_ID              print status.json (bounded)
//   agent-job result JOB_ID              print result.json, Claude's JSON output (bounded)
//   agent-job stop JOB_ID                stop the job (kills its whole cgroup) and wipe its slot
//   agent-job run JOB_ID SLOT            internal: the job itself, as systemd unit agent-job-JOB_ID
//
// `start` returns at once with one JSON line: {"state":"started"} or {"state":"rejected","message":...}.
import { execFileSync, spawn, spawnSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'

const STATE = '/var/lib/agent-jobs'
const CONFIG = '/etc/agent-job/config.json'
const SELF = '/usr/local/lib/agent-run/agent-job.mjs'
const PATH_ENV = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
const FIELDS = ['agent', 'claude_version', 'effort', 'job_id', 'model', 'timeout_s', 'workdir']
const CRED_KEYS = ['CLAUDE_CODE_OAUTH_TOKEN', 'GH_TOKEN']
const MAX_JOB = 4096
const MAX_PROMPT = 262144
const MAX_CRED = 16384
const MAX_RESULT = 4 * 1024 * 1024
const MAX_STDERR = 1024 * 1024
const MAX_PRINT = 65536

class Reject extends Error {}

const stamp = () => new Date().toISOString()
const jobDir = id => path.join(STATE, id)
const slotFile = n => path.join(STATE, 'slots', String(n))
const unitName = id => `agent-job-${id}`

function writeJson(file, obj) {
  const tmp = `${file}.tmp`
  fs.writeFileSync(tmp, JSON.stringify(obj) + '\n', { mode: 0o600 })
  fs.renameSync(tmp, file)
}

function readBounded(file, cap) {
  const fd = fs.openSync(file, 'r')
  try {
    const buf = Buffer.alloc(cap + 1)
    const n = fs.readSync(fd, buf, 0, cap + 1, 0)
    if (n > cap) throw new Reject(`${path.basename(file)} larger than ${cap} bytes`)
    return buf.subarray(0, n)
  } finally {
    fs.closeSync(fd)
  }
}

function validateId(id) {
  if (typeof id !== 'string' || !/^[a-z0-9][a-z0-9-]{0,63}$/.test(id)) throw new Reject('job_id does not match the pattern')
}

function validateJob(bytes, cfg) {
  let job
  try {
    job = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes))
  } catch (e) {
    throw new Reject(`job.json is not strict UTF-8 JSON (${e.name})`)
  }
  if (job === null || typeof job !== 'object' || Array.isArray(job)) throw new Reject('job.json must be one JSON object')
  const have = Object.keys(job).sort()
  if (have.length !== FIELDS.length || have.some((k, i) => k !== FIELDS[i])) {
    throw new Reject(`fields must be exactly ${FIELDS.join(',')}`)
  }
  for (const k of FIELDS) {
    if (k !== 'timeout_s' && typeof job[k] !== 'string') throw new Reject(`${k} must be a string`)
  }
  if (!Number.isInteger(job.timeout_s)) throw new Reject('timeout_s must be an integer')
  validateId(job.job_id)
  if (!/^[a-z0-9][a-z0-9-]{0,39}$/.test(job.workdir)) throw new Reject('workdir does not match the pattern')
  if (!/^[a-z0-9][a-z0-9-]{0,63}$/.test(job.agent)) throw new Reject('agent does not match the pattern')
  if (!/^[0-9]{1,4}\.[0-9]{1,4}\.[0-9]{1,6}$/.test(job.claude_version)) throw new Reject('claude_version does not match the pattern')
  if (!cfg.models.includes(job.model)) throw new Reject('model not allowed')
  if (!cfg.efforts.includes(job.effort)) throw new Reject('effort not allowed')
  if (!cfg.agents.includes(job.agent)) throw new Reject('agent not allowed')
  if (job.timeout_s < 60 || job.timeout_s > cfg.max_timeout_s) throw new Reject('timeout_s out of range')
  return job
}

// cred.env: exactly the two credentials, KEY=VALUE, nothing else.
function validateCred(bytes) {
  const seen = new Set()
  for (const line of bytes.toString('utf8').split(/\r?\n/)) {
    if (line === '') continue
    const m = /^([A-Z_]+)=([\x21-\x7e]+)$/.exec(line)
    if (!m || !CRED_KEYS.includes(m[1]) || seen.has(m[1])) throw new Reject('cred.env must hold exactly CLAUDE_CODE_OAUTH_TOKEN and GH_TOKEN')
    seen.add(m[1])
  }
  if (seen.size !== CRED_KEYS.length) throw new Reject('cred.env must hold exactly CLAUDE_CODE_OAUTH_TOKEN and GH_TOKEN')
}

function slotUser(n) {
  if (!/^[0-9]{1,2}$/.test(String(n))) throw new Reject('slot must be a number')
  const user = `agent${n}`
  if (spawnSync('id', ['-u', user]).status !== 0) throw new Reject(`no slot ${n}`)
  return user
}

function unitActive(id) {
  return spawnSync('systemctl', ['is-active', '--quiet', unitName(id)]).status === 0
}

// Kill everything the slot's user runs, remove its home and its files in the shared temp folders,
// and recreate the empty slot (setup.sh make_slots' layout).
function wipeSlot(n) {
  const user = `agent${n}`
  for (let i = 0; i < 50; i++) {
    spawnSync('pkill', ['-KILL', '-u', user])
    if (spawnSync('pgrep', ['-u', user]).status !== 0) break
    spawnSync('sleep', ['0.1'])
  }
  const home = `/srv/agents/${n}`
  fs.rmSync(home, { recursive: true, force: true })
  for (const d of ['/tmp', '/var/tmp', '/dev/shm']) {
    spawnSync('find', [d, '-xdev', '-mindepth', '1', '-user', user, '-delete'])
  }
  for (const d of ['', '/work', '/tmp', '/logs', '/.cargo']) {
    spawnSync('install', ['-d', '-o', user, '-g', user, '-m', '700', home + d])
  }
  return !fs.existsSync(`${home}/work`) || fs.readdirSync(`${home}/work`).length === 0
}

function cmdStart(args) {
  const slot = args[args.indexOf('--slot') + 1]
  const dir = args[args.indexOf('--dir') + 1]
  if (!args.includes('--slot') || !args.includes('--dir') || !slot || !dir) throw new Reject('usage: start --slot N --dir DIR')
  const credPath = path.join(dir, 'cred.env')
  try {
    const cfg = JSON.parse(fs.readFileSync(CONFIG, 'utf8'))
    const user = slotUser(slot)
    const jobBytes = readBounded(path.join(dir, 'job.json'), MAX_JOB)
    const promptBytes = readBounded(path.join(dir, 'prompt.txt'), MAX_PROMPT)
    if (promptBytes.length === 0) throw new Reject('prompt.txt is empty')
    const credBytes = readBounded(credPath, MAX_CRED)
    const job = validateJob(jobBytes, cfg)
    validateCred(credBytes)
    fs.mkdirSync(path.join(STATE, 'slots'), { recursive: true, mode: 0o700 })
    if (fs.existsSync(jobDir(job.job_id))) throw new Reject('job_id already used')
    if (fs.existsSync(slotFile(slot))) {
      const other = fs.readFileSync(slotFile(slot), 'utf8').trim()
      if (other && unitActive(other)) throw new Reject(`slot ${slot} is busy (${other})`)
    }
    const d = jobDir(job.job_id)
    fs.mkdirSync(d, { mode: 0o700 })
    fs.writeFileSync(path.join(d, 'job.json'), jobBytes, { mode: 0o600 })
    fs.writeFileSync(path.join(d, 'prompt.txt'), promptBytes, { mode: 0o600 })
    fs.writeFileSync(path.join(d, 'cred.env'), credBytes, { mode: 0o600 })
    fs.writeFileSync(slotFile(slot), job.job_id + '\n', { mode: 0o600 })
    writeJson(path.join(d, 'status.json'), { state: 'queued', job_id: job.job_id, slot: Number(slot), user, queued: stamp() })
    const r = spawnSync('systemd-run', [
      `--unit=${unitName(job.job_id)}`, '--collect', '--quiet',
      `--property=RuntimeMaxSec=${job.timeout_s + 300}`, '--property=KillMode=control-group',
      `--setenv=PATH=${PATH_ENV}`,
      process.execPath, SELF, 'run', job.job_id, String(slot),
    ], { encoding: 'utf8' })
    if (r.status !== 0) {
      writeJson(path.join(d, 'status.json'), { state: 'error', job_id: job.job_id, message: 'systemd-run failed', at: stamp() })
      fs.rmSync(path.join(d, 'cred.env'), { force: true })
      throw new Error(`systemd-run failed: ${(r.stderr || '').slice(0, 200)}`)
    }
    console.log(JSON.stringify({ state: 'started', job_id: job.job_id }))
  } finally {
    fs.rmSync(credPath, { force: true })
  }
}

async function cmdRun(id, slot) {
  validateId(id)
  const d = jobDir(id)
  const job = JSON.parse(fs.readFileSync(path.join(d, 'job.json'), 'utf8'))
  const cfg = JSON.parse(fs.readFileSync(CONFIG, 'utf8'))
  const user = slotUser(slot)
  const status = { state: 'setup', job_id: id, slot: Number(slot), user, started: stamp() }
  writeJson(path.join(d, 'status.json'), status)
  const credPath = path.join(d, 'cred.env')
  const envPath = path.join(d, 'job.env')
  let launched = false
  try {
    const v = execFileSync('claude', ['--version'], { encoding: 'utf8', env: { PATH: PATH_ENV, DISABLE_AUTOUPDATER: '1', HOME: '/root' }, timeout: 60000 })
    const have = v.trim().split(/\s+/)[0]
    if (have !== job.claude_version) throw new Reject(`Claude Code is ${have}, the job requires ${job.claude_version}`)
    status.claude_version = have
    if (!wipeSlot(slot)) throw new Error('slot wipe failed')
    // The job's environment: its two credentials plus fixed settings. agent-run copies this file
    // into the slot (readable by the slot's user only), deletes it here, and agent-run.mjs deletes
    // the copy once read.
    const helper = '!f() { test "$1" = get && echo username=x-access-token && echo "password=$GH_TOKEN"; }; f'
    fs.writeFileSync(envPath, [
      fs.readFileSync(credPath, 'utf8').trim(),
      'DISABLE_AUTOUPDATER=1', 'GIT_TERMINAL_PROMPT=0',
      'GIT_CONFIG_COUNT=1', 'GIT_CONFIG_KEY_0=credential.https://github.com.helper', `GIT_CONFIG_VALUE_0=${helper}`,
      '',
    ].join('\n'), { mode: 0o600 })
    fs.rmSync(credPath, { force: true })
    const argv = ['-p', '--model', job.model, '--effort', job.effort]
    if (job.agent !== 'none') argv.push('--agent', job.agent)
    argv.push('--permission-mode', 'bypassPermissions', '--permission-prompts', 'none', '--strict-mcp-config',
      '--no-session-persistence', '--output-format', 'json')
    const script = 'r=$1; w=$2; shift 2; git clone --quiet -- "$r" "$w" 1>&2 || exit 97; cd "$w" || exit 98; exec claude "$@"'
    const child = spawn('/usr/local/bin/agent-run', ['--slot', String(slot), '--env-file', envPath, '--',
      'sh', '-c', script, 'sh', cfg.repo, job.workdir, ...argv], {
      stdio: [fs.openSync(path.join(d, 'prompt.txt'), 'r'), 'pipe', 'pipe'],
      env: { PATH: PATH_ENV },
    })
    launched = true
    status.state = 'running'
    status.pid = child.pid
    writeJson(path.join(d, 'status.json'), status)
    const capture = (stream, file, cap) => {
      const fd = fs.openSync(file, 'w', 0o600)
      let n = 0
      stream.on('data', chunk => {
        if (n >= cap) return
        const part = chunk.subarray(0, cap - n)
        fs.writeSync(fd, part)
        n += part.length
      })
      return () => { fs.closeSync(fd); return n >= cap }
    }
    const closeOut = capture(child.stdout, path.join(d, 'result.json'), MAX_RESULT)
    const closeErr = capture(child.stderr, path.join(d, 'stderr.txt'), MAX_STDERR)
    const outcome = await new Promise(resolve => {
      const timer = setTimeout(() => resolve({ timeout: true }), job.timeout_s * 1000)
      child.on('exit', (code, signal) => { clearTimeout(timer); resolve({ code, signal }) })
    })
    if (outcome.timeout) {
      status.state = 'timeout'
      spawnSync('pkill', ['-KILL', '-u', user])
      child.kill('SIGKILL')
    } else {
      status.state = 'done'
      status.exit_code = outcome.code ?? 128
    }
    status.result_truncated = closeOut()
    closeErr()
  } catch (e) {
    status.state = e instanceof Reject ? 'rejected' : 'error'
    status.message = String(e.message).slice(0, 300)
  } finally {
    fs.rmSync(credPath, { force: true })
    fs.rmSync(envPath, { force: true })
    try {
      fs.copyFileSync(`/srv/agents/${slot}/logs/network.log`, path.join(d, 'network.log'))
    } catch {}
    status.workdir_removed = launched ? wipeSlot(slot) : true
    status.finished = stamp()
    writeJson(path.join(d, 'status.json'), status)
    try {
      if (fs.readFileSync(slotFile(slot), 'utf8').trim() === id) fs.rmSync(slotFile(slot))
    } catch {}
  }
}

function cmdPrint(id, name, cap) {
  validateId(id)
  const f = path.join(jobDir(id), name)
  if (!fs.existsSync(f)) {
    process.exitCode = 3
    return
  }
  process.stdout.write(readBounded(f, cap))
}

function cmdStop(id) {
  validateId(id)
  const d = jobDir(id)
  if (!fs.existsSync(d)) throw new Reject('no such job')
  spawnSync('systemctl', ['stop', unitName(id)])
  const status = JSON.parse(fs.readFileSync(path.join(d, 'status.json'), 'utf8'))
  for (const f of ['cred.env', 'job.env']) fs.rmSync(path.join(d, f), { force: true })
  if (['queued', 'setup', 'running'].includes(status.state)) {
    status.state = 'stopped'
    status.workdir_removed = wipeSlot(status.slot)
    status.finished = stamp()
    writeJson(path.join(d, 'status.json'), status)
    try {
      if (fs.readFileSync(slotFile(status.slot), 'utf8').trim() === id) fs.rmSync(slotFile(status.slot))
    } catch {}
  }
  console.log(JSON.stringify({ state: status.state, job_id: id }))
}

async function main() {
  if (process.getuid() !== 0) throw new Reject('run as root (sudo)')
  fs.mkdirSync(STATE, { recursive: true, mode: 0o700 })
  const [cmd, ...args] = process.argv.slice(2)
  if (cmd === 'start') return cmdStart(args)
  if (cmd === 'run') return cmdRun(args[0], args[1])
  if (cmd === 'status') return cmdPrint(args[0], 'status.json', MAX_PRINT)
  if (cmd === 'result') return cmdPrint(args[0], 'result.json', MAX_RESULT)
  if (cmd === 'stop') return cmdStop(args[0])
  throw new Reject('usage: agent-job start --slot N --dir DIR | status ID | result ID | stop ID')
}

try {
  await main()
} catch (e) {
  console.log(JSON.stringify({ state: e instanceof Reject ? 'rejected' : 'error', message: String(e.message).slice(0, 300) }))
  process.exitCode = 2
}
