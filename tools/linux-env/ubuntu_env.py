"""maxb35t fork: set up and test the Ubuntu agent VM from the Windows host (uses asb.py).

    python ubuntu_env.py sudo    VM          one-time: passwordless sudo for the VM's admin user
                                             (asks for its password once; never stored)
    python ubuntu_env.py install VM [SLOTS]  setup.sh install (needs the VM on NAT for now)
    python ubuntu_env.py proxied VM          setup.sh proxied (then switch the VM to Proxied)
    python ubuntu_env.py test    VM          check isolation once the VM is on Proxied

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

    log = c.proxy_log(vm=vm, limit=200)
    check(any(e.get("host") == "example.com" for e in log), "host Proxy activity shows the slot's example.com")
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
    if what == "test":
        return cmd_test(c, vm)
    sys.exit(__doc__)


if __name__ == "__main__":
    sys.exit(main())
