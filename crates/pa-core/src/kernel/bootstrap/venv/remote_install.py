# Remote kernel install (see remote.rs). Unpacks the uploaded archive
# (stdin) and builds or updates the install under an exclusive flock, so
# concurrent spawns on one host install once and an interrupted install
# (no stamp) is rebuilt by the next one. A crashed installer releases its
# flock with its process, so no stale-lock breaking is ever needed.
#
# Layout under <root>/<key>/: .prime-agent-remote (marks a dir this tool
# created), runtime/, venv/, bin/uv (when shipped), skills/<name>-<hash>/
# with skills/<name>.installed stamps, .installed (the base stamp, written
# last), and .inuse/.last-used (held and touched by running kernels).
#
# argv: install root, base key, JSON {skill dir: content hash} to install,
# JSON extra requirements, "1" when the archive carries the base runtime.
import fcntl
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import time

root = os.path.expanduser(sys.argv[1])
key = sys.argv[2]
skills = json.loads(sys.argv[3])
extras = json.loads(sys.argv[4])
archive_has_base = sys.argv[5] == "1"
target = os.path.join(root, key)
venv = os.path.join(target, "venv")
python = os.path.join(venv, "bin", "python")
stamp = os.path.join(target, ".installed")
MARKER = ".prime-agent-remote"
KEY_PATTERN = re.compile(r"^v[0-9]+-[0-9a-f]{16}$")
LOCK_TIMEOUT_SECONDS = 900
# Garbage collection only removes installs idle for this long.
GC_IDLE_SECONDS = 7 * 24 * 3600

archive = sys.stdin.buffer.read()
os.makedirs(root, exist_ok=True)


def try_lock(path, flags):
    """Open and flock `path`; None when another process holds it."""
    fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(fd, flags | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(fd)
        return None
    # A lock file unlinked (by GC) while we waited is not the live lock.
    try:
        same = os.fstat(fd).st_ino == os.stat(path).st_ino
    except FileNotFoundError:
        same = False
    if not same:
        os.close(fd)
        return False
    return fd


def lock(path):
    deadline = time.monotonic() + LOCK_TIMEOUT_SECONDS
    while True:
        fd = try_lock(path, fcntl.LOCK_EX)
        if fd:
            return fd
        if time.monotonic() > deadline:
            sys.exit(
                "timed out after %ds waiting for the remote install lock %s"
                % (LOCK_TIMEOUT_SECONDS, path)
            )
        if fd is None:
            time.sleep(0.5)


def read(path):
    try:
        with open(path) as handle:
            return handle.read().strip()
    except (OSError, UnicodeDecodeError):
        return None


def write(path, value):
    temp = path + ".tmp"
    with open(temp, "w") as handle:
        handle.write(value)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temp, path)


def run(command):
    code = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=sys.stderr.fileno()).returncode
    if code != 0:
        raise RuntimeError(" ".join(command) + " failed with exit code " + str(code))


def collect_garbage():
    """Remove other installs this tool created that no kernel uses: each
    running kernel holds a shared flock on its install's .inuse, and only
    installs idle past GC_IDLE_SECONDS whose install lock is free go."""
    now = time.time()
    for name in sorted(os.listdir(root)):
        path = os.path.join(root, name)
        if name == key or not KEY_PATTERN.match(name) or not os.path.isdir(path):
            continue
        if read(os.path.join(path, MARKER)) != name:
            continue
        last_used = max(
            (
                os.stat(os.path.join(path, entry)).st_mtime
                for entry in (".last-used", ".installed", MARKER)
                if os.path.exists(os.path.join(path, entry))
            ),
            default=now,
        )
        if now - last_used < GC_IDLE_SECONDS:
            continue
        install_lock = try_lock(path + ".lock", fcntl.LOCK_EX)
        if not install_lock:
            continue
        try:
            in_use = try_lock(os.path.join(path, ".inuse"), fcntl.LOCK_EX)
            if not in_use:
                continue
            try:
                shutil.rmtree(path)
                print("removed unused remote install " + path, file=sys.stderr)
            finally:
                os.close(in_use)
            os.unlink(path + ".lock")
        finally:
            os.close(install_lock)


lock_fd = lock(target + ".lock")
try:
    base_current = os.path.isfile(python) and read(stamp) == key
    if not base_current:
        if not archive_has_base:
            sys.exit("the remote install " + target + " disappeared since the probe; retry the spawn")
        shutil.rmtree(target, ignore_errors=True)
        os.makedirs(target)
        write(os.path.join(target, MARKER), key)
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as bundle:
        if hasattr(tarfile, "data_filter"):
            bundle.extractall(target, filter="data")
        else:
            bundle.extractall(target)
    uploaded_uv = os.path.join(target, "bin", "uv")
    uv = shutil.which("uv") or (uploaded_uv if os.path.isfile(uploaded_uv) else None)
    runtime = os.path.join(target, "runtime")

    def pip_install(arguments):
        if uv:
            run([uv, "pip", "install", "--quiet", "--python", python] + arguments)
        else:
            run([python, "-m", "pip", "install", "--quiet", "--disable-pip-version-check"] + arguments)

    if not base_current:
        if uv:
            run([uv, "venv", "--quiet", "--python", sys.executable, venv])
        else:
            run([sys.executable, "-m", "venv", venv])
        pip_install([runtime] + extras)
        write(stamp, key)
    def source(name):
        return os.path.join(target, "skills", name + "-" + skills[name])

    def stamp_skill(name):
        skill_stamp = os.path.join(target, "skills", name + ".installed")
        previous = read(skill_stamp)
        write(skill_stamp, skills[name])
        if previous and previous != skills[name]:
            shutil.rmtree(os.path.join(target, "skills", name + "-" + previous), ignore_errors=True)

    # Editable like the local kernel venv: a changed skill is a new source
    # dir, which replaces the previous editable install. The runtime rides
    # along so a skill depending on it resolves here. One batch first (a
    # skill may depend on a sibling), then per skill: a failing skill warns
    # and the rest still install, like the local sync.
    names = sorted(skills)
    failed = []
    batch = [runtime]
    for name in names:
        batch += ["--editable", source(name)]
    try:
        if names:
            pip_install(batch)
        for name in names:
            stamp_skill(name)
    except Exception:
        for name in names:
            try:
                pip_install([runtime, "--editable", source(name)])
                stamp_skill(name)
            except Exception as error:
                failed.append({"name": name, "error": str(error)})
    try:
        collect_garbage()
    except OSError as error:
        print("remote install garbage collection skipped: " + str(error), file=sys.stderr)
    print(json.dumps({"installed_base": not base_current, "failed_skills": failed}))
finally:
    os.close(lock_fd)
