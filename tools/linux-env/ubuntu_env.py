"""maxb35t fork: set up and test the Ubuntu agent VM from the Windows host (uses asb.py).

    python ubuntu_env.py sudo    VM          one-time: passwordless sudo for the VM's admin user
                                             (asks for its password once; never stored)
    python ubuntu_env.py install VM [SLOTS]  setup.sh install (needs the VM on NAT for now)
    python ubuntu_env.py proxied VM          setup.sh proxied (then switch the VM to Proxied)
    python ubuntu_env.py userns  VM          setup.sh userns (user-namespace settings, part of install)
    python ubuntu_env.py jobs    VM          setup.sh jobs (engine job pieces and pinned versions, part of install)
    python ubuntu_env.py test    VM          check isolation once the VM is on Proxied
    python ubuntu_env.py jobtest VM [SLOT]   one real engine job through agent-job (needs the agent's
                                             tokens; run elevated, they're read from C:\engine-agent\cred)

REF selects the fork branch or tag the VM fetches setup files from (default: main).
"""
import getpass
import os
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, r"C:\ProgramData\AgentHost")
import asb  # noqa: E402

REF = os.environ.get("ASB_LINUX_ENV_REF", "main")
RAW = "https://raw.githubusercontent.com/maxb35t/appsandbox/%s/tools/linux-env/setup.sh" % REF


def ready(c, vm):
    if c.ssh_info(vm).get("sshState") != 4:
        sys.exit("SSH to %s isn't ready yet (start it and wait for 'online')." % vm)


def show(code, out, err):
    sys.stdout.write(out)
    if err.strip():
        sys.stdout.write(err)
    print("\n(exit %d)" % code)
    return code


def cmd_sudo(c, vm):
    ready(c, vm)
    pw = getpass.getpass("Password of %s's admin user (used once, not stored): " % vm)
    code, out, err = c.run(vm, "sudo -S -p '' sh -c 'echo \"$SUDO_USER ALL=(ALL) NOPASSWD:ALL\" "
                               "> /etc/sudoers.d/90-asb-admin && chmod 440 /etc/sudoers.d/90-asb-admin "
                               "&& visudo -cq && echo sudo-ok'", input=pw + "\n")
    pw = None
    print((out + err).strip())
    return 0 if "sudo-ok" in out else 1


def fetch_and_run(c, vm, args, timeout):
    ready(c, vm)
    # Ubuntu Desktop has wget but not curl (setup.sh installs curl for itself).
    cmd = ("(command -v curl >/dev/null && curl -fsSL %s -o /tmp/asb-setup.sh || wget -q -O /tmp/asb-setup.sh %s) "
           "&& sudo -n bash /tmp/asb-setup.sh %s; rc=$?; rm -f /tmp/asb-setup.sh; exit $rc") % (RAW, RAW, args)
    print("running in %s: setup.sh %s (this can take a while)" % (vm, args), flush=True)
    return show(*c.run(vm, cmd, timeout=timeout))


def cmd_test(c, vm):
    ok = True
    t0 = time.monotonic()

    def check(cond, label):
        nonlocal ok
        ok &= bool(cond)
        print("[%6.1fs] %s  %s" % (time.monotonic() - t0, "PASS" if cond else "FAIL", label), flush=True)

    def sh(cmd, timeout=120):
        code, out, err = c.run(vm, "bash -lc %s" % _q(cmd), timeout=timeout)
        return code, (out + err).strip()

    ready(c, vm)
    st = c.status(vm)
    check(st.get("networkMode") == 4, "VM network is Proxied (networkMode %s)" % st.get("networkMode"))
    code, out = sh("systemctl is-active asb-relay")
    check(out == "active", "guest relay running (%s)" % out)
    code, out = sh("ip -o link show | awk -F': ' '{print $2}' | grep -v '^lo$' | tr '\\n' ' '")
    check(out == "", "no network adapter besides loopback (%r)" % out)
    code, out = sh("curl -s -m 20 -o /dev/null -w '%{http_code}' https://www.google.com")
    check(out == "200", "web through the relay works (google %s)" % out)
    code, out = sh("curl -s -m 8 --noproxy '*' -o /dev/null -w '%{http_code}' https://1.1.1.1; echo")
    check(out.strip() in ("000", ""), "no direct route out (%r)" % out)
    code, out = sh("curl -s -m 20 http://192.168.1.1/")
    check("private-address" in out, "LAN refused by the host proxy (%r)" % out[:60])

    # Slots: identity, own folder, others hidden, shared /tmp hidden, web via srt
    code, out = sh("sudo -n agent-run --slot 1 -- sh -c 'id -un; echo x > ~/work/probe && echo own-ok; "
                   "ls /srv/agents; ls /srv/agents/2 2>&1 | head -1; ls /tmp | wc -l; "
                   "curl -s -m 20 -o /dev/null -w \"%{http_code}\\n\" https://example.com'")
    lines = out.splitlines()
    check(lines[:1] == ["agent1"], "slot 1 runs as agent1 (%r)" % lines[:1])
    check("own-ok" in lines, "slot 1 writes its own folder")
    check("1" in lines and "2" not in lines, "slot 1 sees only its own folder under /srv/agents")
    check(any("No such file" in l for l in lines), "slot 2's folder is hidden from slot 1")
    check("0" in lines, "shared /tmp hidden inside the sandbox")
    check(lines[-1:] == ["200"], "web from inside the sandbox (example.com %s)" % lines[-1:])
    code, out = sh("sudo -n tail -n 5 /srv/agents/1/logs/network.log")
    check("example.com" in out, "slot 1's connection log has example.com")
    code, out = sh("sudo -n agent-run --slot 2 -- sh -c 'cat /srv/agents/1/work/probe 2>&1; "
                   "echo y > /srv/agents/1/work/evil 2>&1 || echo write-refused'")
    check("No such file" in out and "write-refused" in out, "slot 2 can't read or write slot 1 (%r)" % out[:80])

    # Token by env-file: reaches the command, never on a command line, file removed
    # Written with Windows line endings (text mode), as a host-side job would: agent-run accepts both.
    with tempfile.NamedTemporaryFile("w", delete=False, suffix=".env") as f:
        f.write("JOB_TOKEN=test-token-123\n")
        local = f.name
    try:
        sh("mkdir -p ~/jobs")
        c.put(vm, local, "jobs/test.env")
    finally:
        os.unlink(local)
    code, out = sh("sudo -n agent-run --slot 1 --env-file ~/jobs/test.env -- sh -c 'echo token=$JOB_TOKEN'; "
                   "ls ~/jobs/test.env 2>&1 | head -1")
    check("token=test-token-123" in out and "No such file" in out,
          "token via --env-file reaches the command and the file is removed")

    # Two slots at once
    results = {}

    def slot_job(n):
        t = time.monotonic()
        results[n] = (sh("sudo -n agent-run --slot %d -- sh -c 'sleep 5; echo done%d'" % (n, n)),
                      time.monotonic() - t)

    threads = [threading.Thread(target=slot_job, args=(n,)) for n in (1, 2)]
    t = time.monotonic()
    for th in threads:
        th.start()
    for th in threads:
        th.join()
    both = time.monotonic() - t
    check(all(("done%d" % n) in results[n][0][1] for n in (1, 2)) and both < 9,
          "two slots run at the same time (%.1f s for two 5 s jobs)" % both)

    # The daemon's own spelling of the name: older builds filter the log case-sensitively.
    log = c.proxy_log(vm=st.get("name", vm), limit=200)
    check(any(e.get("host") == "example.com" for e in log), "host Proxy activity shows the slot's example.com")
    print("\nALL PASS" if ok else "\nSOME CHECKS FAILED")
    return 0 if ok else 1


CRED_DIR = r"C:\engine-agent\cred"
CLAUDE_VERSION = os.environ.get("ASB_CLAUDE_VERSION", "2.1.284")


def cmd_jobtest(c, vm, slot):
    """A real job through agent-job: clone the engine repo with the agent's GitHub token, run
    `claude -p` with a trivial prompt, then check the records, the slot wipe and that no credential
    is left. Tokens are read on the host, copied in a file that agent-job deletes, never printed."""
    import json
    import secrets
    ok = True
    t0 = time.monotonic()

    def check(cond, label):
        nonlocal ok
        ok &= bool(cond)
        print("[%6.1fs] %s  %s" % (time.monotonic() - t0, "PASS" if cond else "FAIL", label), flush=True)

    def sh(cmd, timeout=120):
        code, out, err = c.run(vm, "bash -lc %s" % _q(cmd), timeout=timeout)
        return code, (out + err).strip()

    ready(c, vm)
    try:
        claude_tok = open(os.path.join(CRED_DIR, "claude-oauth-token"), encoding="utf-8").read().strip()
        gh_tok = open(os.path.join(CRED_DIR, "github-token"), encoding="utf-8").read().strip()
    except OSError as e:
        sys.exit("can't read the agent's tokens in %s (run elevated): %s" % (CRED_DIR, e.strerror))
    job_id = "jobtest-" + secrets.token_hex(6)
    job = {"agent": "none", "claude_version": CLAUDE_VERSION, "effort": "low", "job_id": job_id,
           "model": "sonnet", "timeout_s": 600, "workdir": "w-" + job_id[-12:]}
    local = tempfile.mkdtemp(prefix="jobtest-")
    try:
        with open(os.path.join(local, "job.json"), "w", newline="\n") as f:
            f.write(json.dumps(job, separators=(",", ":")))
        with open(os.path.join(local, "prompt.txt"), "w", newline="\n") as f:
            f.write("Reply with exactly the word ok, then stop. Do not change any file.\n")
        with open(os.path.join(local, "cred.env"), "w", newline="\n") as f:
            f.write("CLAUDE_CODE_OAUTH_TOKEN=%s\nGH_TOKEN=%s\n" % (claude_tok, gh_tok))
        sh("mkdir -p ~/jobs")
        c.put(vm, local, "jobs/" + job_id)
    finally:
        for n in os.listdir(local):
            os.unlink(os.path.join(local, n))
        os.rmdir(local)
    code, out = sh("sudo -n agent-job start --slot %d --dir ~/jobs/%s; ls ~/jobs/%s" % (slot, job_id, job_id))
    check('"state":"started"' in out, "job %s started in slot %d (%s)" % (job_id, slot, out.splitlines()[0][:120]))
    check("cred.env" not in out, "cred.env deleted from the hand-over folder at start")
    status = {}
    for _ in range(300):
        code, out = sh("sudo -n agent-job status %s" % job_id)
        try:
            status = json.loads(out)
        except ValueError:
            status = {}
        if status.get("state") not in ("queued", "setup", "running"):
            break
        time.sleep(2)
    check(status.get("state") == "done" and status.get("exit_code") == 0,
          "job finished: state %s, exit %s, Claude Code %s" % (status.get("state"), status.get("exit_code"),
                                                              status.get("claude_version")))
    code, out = sh("sudo -n agent-job result %s" % job_id, timeout=120)
    try:
        res = json.loads(out)
    except ValueError:
        res = {}
    models = sorted((res.get("modelUsage") or {}).keys())
    check(res.get("is_error") is False and "ok" in str(res.get("result", "")).lower(),
          "claude -p answered (%r, models %s)" % (str(res.get("result", ""))[:40], models))
    check(status.get("workdir_removed") is True, "slot wiped after the job")
    code, out = sh("sudo -n ls -A /srv/agents/%d/work /srv/agents/%d/tmp; sudo -n ls /var/lib/agent-jobs/%s; "
                   "pgrep -u agent%d -c || true" % (slot, slot, job_id, slot))
    check("cred.env" not in out and "job.env" not in out and ".job-env" not in out,
          "no credential file left in the record or the slot")
    check(out.strip().endswith("0"), "no process left for agent%d" % slot)
    code, out = sh("sudo -n cat /var/lib/agent-jobs/%s/network.log | awk '{print $2}' | sort -u | tr '\\n' ' '" % job_id)
    check("github.com:443" in out, "the slot's connection log has github.com (%s)" % out[:120])
    print("\nALL PASS" if ok else "\nSOME CHECKS FAILED")
    return 0 if ok else 1


def _q(s):
    return "'" + s.replace("'", "'\\''") + "'"


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    what, vm = sys.argv[1], sys.argv[2]
    c = asb.connect()
    if what == "sudo":
        return cmd_sudo(c, vm)
    if what == "install":
        slots = sys.argv[3] if len(sys.argv) > 3 else "4"
        return fetch_and_run(c, vm, "install %s %s" % (REF, slots), timeout=3600)
    if what == "proxied":
        return fetch_and_run(c, vm, "proxied", timeout=600)
    if what == "jobs":
        return fetch_and_run(c, vm, "jobs %s" % REF, timeout=3600)
    if what == "userns":
        return fetch_and_run(c, vm, "userns", timeout=300)
    if what == "jobtest":
        return cmd_jobtest(c, vm, int(sys.argv[3]) if len(sys.argv) > 3 else 1)
    if what == "test":
        return cmd_test(c, vm)
    sys.exit(__doc__)


if __name__ == "__main__":
    sys.exit(main())
