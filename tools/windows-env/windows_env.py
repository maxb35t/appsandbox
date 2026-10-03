"""maxb35t fork: environment 2 (Windows, no desktop) -- jobs in throwaway instances of the
Windows agent VM with the GPU off and no display, run over SSH in session 0. Uses asb.py.

    python windows_env.py run  [OPTIONS] [--env-file F] -- COMMAND   one job in a fresh instance
    python windows_env.py test [OPTIONS]                              isolation + toolchain checks
    python windows_env.py measure [OPTIONS] [--parallel N]            build timings and memory

OPTIONS: --vm NAME (default AgentTest)  --snap NAME (default windows-agent-base-v1)
         --ram MB (default 6144)  --cores N (default: the VM's)  --gpu (environment 3: GPU on)

`run` copies the env file (KEY=VALUE lines, e.g. a job token) into the instance, where the job
loads it into its environment and deletes it before the command starts; the command runs with
cmd.exe in C:\\job. The instance is created with fast stop and auto-delete, so it is gone when
the job ends. Exit code: the command's.
"""
import os
import shutil
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, r"C:\ProgramData\AgentHost")
import asb  # noqa: E402

JOB = "C:/job"


class Opts:
    vm = "AgentTest"
    snap = "windows-agent-base-v1"
    ram = 6144
    cores = None
    gpu = False
    env_file = None
    parallel = 1
    command = []


def parse(argv):
    o = Opts()
    o.command = []
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--":
            o.command = argv[i + 1:]
            break
        if a in ("--vm", "--snap", "--ram", "--cores", "--env-file", "--parallel") and i + 1 < len(argv):
            v = argv[i + 1]
            if a == "--vm": o.vm = v
            elif a == "--snap": o.snap = v
            elif a == "--ram": o.ram = int(v)
            elif a == "--cores": o.cores = int(v)
            elif a == "--env-file": o.env_file = v
            else: o.parallel = int(v)
            i += 2
            continue
        if a == "--gpu":
            o.gpu = True
            i += 1
            continue
        sys.exit("unknown option %s\n%s" % (a, __doc__))
    return o


def snap_index(c, o):
    for s in c.snapshots(o.vm):
        if s.get("name") == o.snap:
            return s["index"]
    sys.exit("%s has no snapshot named %r (%s)" % (o.vm, o.snap, [s.get("name") for s in c.snapshots(o.vm)]))


def ps(c, name, script, timeout=1800):
    code, out, err = c.run_ps(name, "$ErrorActionPreference='Continue'\n" + script, timeout=timeout)
    return code, (out + err).strip()


class Instance:
    """A throwaway instance, online with SSH ready; removed on exit (fast stop + auto-delete)."""

    def __init__(self, c, o, ttl_minutes=120):
        self.c, self.o, self.name = c, o, None
        self.ttl = ttl_minutes
        self.ready_s = None

    def __enter__(self):
        t = time.monotonic()
        st, b = self.c.create_instance(self.o.vm, snap_index(self.c, self.o), ram_mb=self.o.ram,
                                       cpu_cores=self.o.cores, gpu_mode=None if self.o.gpu else 0,
                                       auto_delete=True, fast_stop=True, ttl_minutes=self.ttl)
        if st not in (200, 201, 202) or not b.get("name"):
            raise RuntimeError("couldn't create an instance of %s: %s %s" % (self.o.vm, st, b))
        self.name = b["name"]
        self.c.wait(self.name, {"online"}, 600)
        while self.c.ssh_info(self.name).get("sshState") != 4:
            if time.monotonic() - t > 900:
                raise TimeoutError("SSH to %s not ready after 15 min" % self.name)
            time.sleep(1)
        self.ready_s = time.monotonic() - t
        code, out = ps(self.c, self.name, "New-Item -ItemType Directory -Force '%s' | Out-Null" % JOB)
        if code != 0:
            raise RuntimeError("couldn't create %s in %s: %s" % (JOB, self.name, out))
        return self

    def __exit__(self, *exc):
        if self.name and self.name in [v["name"] for v in self.c.list()]:
            self.c.shutdown(self.name)
            for _ in range(120):
                if self.name not in [v["name"] for v in self.c.list()]:
                    break
                time.sleep(1)
        return False


# Loads C:\job\.job-env into the environment, deletes it, then runs the command with cmd.exe.
_RUN = r"""
$f = 'C:\job\.job-env'
if (Test-Path $f) {
    foreach ($l in Get-Content $f) {
        if ($l -match '^\s*([A-Za-z_][A-Za-z0-9_]*)=(.*)$') { Set-Item -Path ('env:' + $matches[1]) -Value $matches[2] }
    }
    Remove-Item $f -Force
}
Set-Location C:\job
cmd /d /c '@CMD@'
exit $LASTEXITCODE
"""


def job(c, name, command, env_file=None, timeout=3600):
    """Run `command` (a cmd.exe line) in C:\\job of instance `name`. Returns (code, output)."""
    if env_file:
        c.put(name, env_file, JOB + "/.job-env")
    return ps(c, name, _RUN.replace("@CMD@", command.replace("'", "''")), timeout=timeout)


def cmd_run(c, o):
    if not o.command:
        sys.exit("run: no command (put it after --)")
    command = " ".join(o.command)
    with Instance(c, o) as inst:
        print("instance %s ready in %.0f s" % (inst.name, inst.ready_s), file=sys.stderr, flush=True)
        try:
            code, out = job(c, inst.name, command, o.env_file)
        finally:
            if o.env_file:
                os.unlink(o.env_file)
        print(out)
    return code


# A crate with a few real dependencies (fetched from crates.io through the proxy), a test, and
# enough code to make the MSVC linker do real work.
_CARGO_TOML = """[package]
name = "envcheck"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
regex = "1"
"""
_MAIN_RS = """use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct Job { id: u32, name: String }

fn parse(s: &str) -> Job { serde_json::from_str(s).unwrap() }

fn main() {
    let re = regex::Regex::new(r"^[a-z]+-[0-9]+$").unwrap();
    println!("{} {:?}", re.is_match("agent-1"), parse(r#"{"id":1,"name":"a"}"#));
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let j = super::Job { id: 7, name: "x".into() };
        assert_eq!(super::parse(&serde_json::to_string(&j).unwrap()), j);
    }
}
"""


def put_crate(c, name):
    d = tempfile.mkdtemp(prefix="envcheck-")
    try:
        root = os.path.join(d, "envcheck")
        os.makedirs(os.path.join(root, "src"))
        with open(os.path.join(root, "Cargo.toml"), "w", newline="\n") as f:
            f.write(_CARGO_TOML)
        with open(os.path.join(root, "src", "main.rs"), "w", newline="\n") as f:
            f.write(_MAIN_RS)
        c.put(name, root, JOB + "/")
    finally:
        shutil.rmtree(d, ignore_errors=True)


_MEM = ("$o = Get-CimInstance Win32_OperatingSystem; "
        "'{0} {1}' -f [int](($o.TotalVisibleMemorySize - $o.FreePhysicalMemory) / 1024), "
        "[int]($o.TotalVisibleMemorySize / 1024)")


def used_mb(c, name):
    code, out = ps(c, name, _MEM)
    try:
        used, total = (int(x) for x in out.split()[-2:])
        return used, total
    except ValueError:
        return -1, -1


def build(c, name):
    """cargo test --release of the check crate; returns (ok, seconds, tail of output)."""
    put_crate(c, name)
    t = time.monotonic()
    code, out = job(c, name, "cd envcheck && cargo test --release 2>&1")
    return code == 0 and "test result: ok" in out, time.monotonic() - t, out[-400:]


def cmd_test(c, o):
    ok = True
    t0 = time.monotonic()

    def check(cond, label):
        nonlocal ok
        ok &= bool(cond)
        print("[%6.1fs] %s  %s" % (time.monotonic() - t0, "PASS" if cond else "FAIL", label), flush=True)

    base_before = c.snapshots(o.vm)
    with Instance(c, o) as inst:
        n = inst.name
        check(True, "instance %s of %s online with SSH in %.0f s (%d MB, GPU %s)"
              % (n, o.vm, inst.ready_s, o.ram, "on" if o.gpu else "off"))
        st = c.status(n)
        check(st.get("networkMode") == 4, "network is Proxied (networkMode %s)" % st.get("networkMode"))
        check(not st.get("displayOpen"), "no display window open")
        code, out = ps(c, n, "@(Get-NetAdapter -ErrorAction SilentlyContinue).Count")
        check(out.strip() == "0", "no network adapter (%r)" % out)
        code, out = ps(c, n, "(Get-CimInstance Win32_VideoController | Select-Object -Expand Name) -join ' / '")
        has_nv = "NVIDIA" in out.upper()
        check(has_nv == o.gpu, "GPU %s as asked (%s)" % ("present" if o.gpu else "absent", out))
        code, out = ps(c, n, "(Get-Process -Id $PID).SessionId")
        check(out.strip() == "0", "jobs run in session 0, no desktop needed (%r)" % out)
        used, total = used_mb(c, n)
        check(used > 0, "memory at idle: %d of %d MB in use" % (used, total))

        code, out = job(c, n, 'curl.exe -s -m 20 -o NUL -w "%{http_code}" https://www.google.com')
        check(out.strip() == "200", "web through the proxy (google %s)" % out.strip())
        code, out = job(c, n, "curl.exe -s -m 20 http://192.168.1.1/")
        check("private-address" in out, "LAN refused by the host proxy (%r)" % out[:60])
        code, out = job(c, n, "git clone -q --depth 1 https://github.com/octocat/Hello-World.git hw && dir /b hw")
        check(code == 0 and "README" in out, "git clone through the proxy (%r)" % out[-80:])

        ok_build, secs, tail = build(c, n)
        check(ok_build, "Rust crate with crates.io deps builds and its test passes (cargo test --release, %.0f s)"
              % secs)
        if not ok_build:
            print(tail)
        used2, _ = used_mb(c, n)
        check(used2 > 0, "memory after the build: %d MB in use" % used2)

        with tempfile.NamedTemporaryFile("w", delete=False, suffix=".env") as f:
            f.write("JOB_TOKEN=test-token-123\n")
            local = f.name
        try:
            code, out = job(c, n, "echo token=%JOB_TOKEN% & if exist .job-env (echo still-there) else (echo removed)",
                            env_file=local)
        finally:
            os.unlink(local)
        check("token=test-token-123" in out and "removed" in out,
              "token via env file reaches the command and the file is removed (%r)" % out[-60:])
    gone = n not in [v["name"] for v in c.list()]
    check(gone, "instance deleted after the job")
    check(c.snapshots(o.vm) == base_before and c.status(o.vm).get("state") == "stopped",
          "%s and its snapshots untouched" % o.vm)
    print("\nALL PASS" if ok else "\nSOME CHECKS FAILED")
    return 0 if ok else 1


def cmd_measure(c, o):
    results = {}

    def one(k):
        try:
            with Instance(c, o) as inst:
                idle, total = used_mb(c, inst.name)
                ok_build, secs, tail = build(c, inst.name)
                after, _ = used_mb(c, inst.name)
                results[k] = (inst.name, inst.ready_s, idle, after, total, ok_build, secs, tail)
        except Exception as e:  # report it with the others
            results[k] = (None, 0, 0, 0, 0, False, 0, str(e))

    t = time.monotonic()
    threads = [threading.Thread(target=one, args=(k,)) for k in range(o.parallel)]
    for th in threads:
        th.start()
        time.sleep(2)
    for th in threads:
        th.join()
    print("%d instance(s), %d MB each, GPU %s; wall time %.0f s"
          % (o.parallel, o.ram, "on" if o.gpu else "off", time.monotonic() - t))
    print("%-14s %8s %8s %8s %8s %6s %8s" % ("instance", "ready s", "idle MB", "after MB", "total", "build", "build s"))
    bad = 0
    for k in sorted(results):
        name, ready, idle, after, total, ok_build, secs, tail = results[k]
        print("%-14s %8.0f %8d %8d %8d %6s %8.0f" % (name, ready, idle, after, total, "ok" if ok_build else "FAIL", secs))
        if not ok_build:
            bad += 1
            print("   ", tail.strip()[-300:])
    return 1 if bad else 0


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("run", "test", "measure"):
        sys.exit(__doc__)
    o = parse(sys.argv[2:])
    c = asb.connect()
    return {"run": cmd_run, "test": cmd_test, "measure": cmd_measure}[sys.argv[1]](c, o)


if __name__ == "__main__":
    sys.exit(main())
