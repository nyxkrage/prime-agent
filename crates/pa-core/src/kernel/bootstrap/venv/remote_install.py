# Remote kernel install (see remote.rs). Unpacks the uploaded archive
# (stdin) and builds or updates the install under an exclusive flock, so
# concurrent spawns on one host install once and an interrupted install
# (no stamp) is rebuilt by the next one. A crashed installer releases its
# flock with its process, so no stale-lock breaking is ever needed.
#
# Layout under <root>/:
# - <key>/: .prime-agent-remote (marks a dir this tool created), runtime/,
#   venv/, bin/uv (when shipped), .installed (the base stamp, written
#   last), skill-sites/<id>/ (a Python skill built with --target, stamped
#   by its .installed), and .inuse/.last-used (held and touched by running
#   kernels);
# - skills/<id>/: one skill dir's files, stamped by .complete; an id is the
#   dir's name plus its content hash, so a changed skill is a new dir and
#   every kernel keeps the version it started with (a pinned sys.path, see
#   rlm/remote.py). Shared by every install, guarded by skills.lock;
# - install-history.log (one line per base build) and run/ (remote kernel
#   records and orphan journals, see rlm/remote.py).
# Nothing a kernel may still use is removed inline: an install, skill dir,
# or skill site goes only through collect_garbage(), once idle and unheld.
#
# argv: install root, base key, JSON {skill id: is Python} (every skill of
# the session), JSON extra requirements, "1" when the archive carries the
# base runtime. The archive's paths are relative to the root.
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
store_root = os.path.join(root, "skills")
site_root = os.path.join(target, "skill-sites")
MARKER = ".prime-agent-remote"
KEY_PATTERN = re.compile(r"^v[0-9]+-[0-9a-f]{16}$")
SKILL_ID_PATTERN = re.compile(r"^[A-Za-z0-9_-]+-[0-9a-f]{16}$")
LOCK_TIMEOUT_SECONDS = 900
# Garbage collection only removes installs and skill dirs idle this long.
GC_IDLE_SECONDS = 7 * 24 * 3600

archive = sys.stdin.buffer.read()
os.makedirs(store_root, exist_ok=True)


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


def extract(bundle, members):
    if hasattr(tarfile, "data_filter"):
        bundle.extractall(root, members=members, filter="data")
    else:
        bundle.extractall(root, members=members)


def remove_if_idle_and_unheld(path, stamps):
    """Remove `path` when nothing touched it for GC_IDLE_SECONDS and no
    running kernel holds its .inuse (a kernel's shared flock blocks this
    exclusive one)."""
    now = time.time()
    last_used = max(
        (
            os.stat(os.path.join(path, entry)).st_mtime
            for entry in (".last-used",) + stamps
            if os.path.exists(os.path.join(path, entry))
        ),
        default=now,
    )
    if now - last_used < GC_IDLE_SECONDS:
        return False
    in_use = try_lock(os.path.join(path, ".inuse"), fcntl.LOCK_EX)
    if not in_use:
        return False
    try:
        shutil.rmtree(path)
        print("removed unused remote install dir " + path, file=sys.stderr)
    finally:
        os.close(in_use)
    return True


def collect_garbage():
    """Remove what this tool created and no kernel uses: other installs,
    this install's skill sites, and skill dirs, each once idle and unheld.
    The current session's own skills are always kept."""
    for name in sorted(os.listdir(root)):
        path = os.path.join(root, name)
        if name == key or not KEY_PATTERN.match(name) or not os.path.isdir(path):
            continue
        if read(os.path.join(path, MARKER)) != name:
            continue
        install_lock = try_lock(path + ".lock", fcntl.LOCK_EX)
        if not install_lock:
            continue
        try:
            if remove_if_idle_and_unheld(path, (".installed", MARKER)):
                os.unlink(path + ".lock")
        finally:
            os.close(install_lock)
    if os.path.isdir(site_root):
        for name in sorted(os.listdir(site_root)):
            path = os.path.join(site_root, name)
            if name in skills or not SKILL_ID_PATTERN.match(name) or not os.path.isdir(path):
                continue
            remove_if_idle_and_unheld(path, (".installed",))
    store_lock = try_lock(os.path.join(root, "skills.lock"), fcntl.LOCK_EX)
    if not store_lock:
        return
    try:
        for name in sorted(os.listdir(store_root)):
            path = os.path.join(store_root, name)
            if name in skills or not SKILL_ID_PATTERN.match(name) or not os.path.isdir(path):
                continue
            if read(os.path.join(path, ".complete")) != name:
                continue
            remove_if_idle_and_unheld(path, (".complete",))
    finally:
        os.close(store_lock)


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
        members = bundle.getmembers()
        extract(bundle, [member for member in members if member.name.startswith(key + "/")])
        # Skill dirs are shared by every install: one writer at a time, and
        # a complete dir is never rewritten (a kernel may be reading it).
        store_lock = lock(os.path.join(root, "skills.lock"))
        try:
            for skill_id in sorted(skills):
                store = os.path.join(store_root, skill_id)
                if read(os.path.join(store, ".complete")) == skill_id:
                    continue
                prefix = "skills/" + skill_id + "/"
                uploaded = [member for member in members if member.name.startswith(prefix)]
                if not uploaded:
                    sys.exit("the remote skill dir " + store + " disappeared since the probe; retry the spawn")
                # An incomplete dir is a crashed install's: no kernel holds it.
                shutil.rmtree(store, ignore_errors=True)
                extract(bundle, uploaded)
                write(os.path.join(store, ".complete"), skill_id)
        finally:
            os.close(store_lock)
    uploaded_uv = os.path.join(target, "bin", "uv")
    uv = shutil.which("uv")
    if uv is None and os.path.isfile(uploaded_uv):
        # The uv this tool shipped keeps its cache inside the install root,
        # so provisioning writes nothing outside it.
        uv = uploaded_uv
        os.environ.setdefault("UV_CACHE_DIR", os.path.join(root, "uv-cache"))
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
        # One line per base build: the install history an operator (or a
        # test) reads to see when and how often this host built a venv.
        with open(os.path.join(root, "install-history.log"), "a") as history:
            history.write("%d built %s\n" % (int(time.time()), key))

    def store(skill_id):
        return os.path.join(store_root, skill_id)

    def site(skill_id):
        return os.path.join(site_root, skill_id)

    def build_site(skill_id):
        """Build the skill (no deps) into its own pinned site dir: the code
        its kernels import, whatever version another session installs."""
        temp = site(skill_id) + ".tmp"
        shutil.rmtree(temp, ignore_errors=True)
        pip_install(["--no-deps", "--target", temp, store(skill_id)])
        write(os.path.join(temp, ".installed"), skill_id)
        # A leftover without its stamp is an interrupted build: unheld.
        shutil.rmtree(site(skill_id), ignore_errors=True)
        os.replace(temp, site(skill_id))

    # The skills' dependencies go into the shared venv (one batch first, as
    # a skill may depend on a sibling; the runtime rides along so a skill
    # depending on it resolves here); the skills' own code goes into the
    # pinned sites. A failing skill warns and the rest still install, like
    # the local sync.
    names = sorted(
        skill_id
        for skill_id, is_python in skills.items()
        if is_python and read(os.path.join(site(skill_id), ".installed")) != skill_id
    )
    os.makedirs(site_root, exist_ok=True)
    failed = []
    try:
        if names:
            pip_install([runtime] + [store(name) for name in names])
        for name in names:
            build_site(name)
    except Exception:
        for name in names:
            if read(os.path.join(site(name), ".installed")) == name:
                continue
            try:
                pip_install([runtime, store(name)])
                build_site(name)
            except Exception as error:
                failed.append({"name": name, "error": str(error)})
    try:
        collect_garbage()
    except OSError as error:
        print("remote install garbage collection skipped: " + str(error), file=sys.stderr)
    print(json.dumps({"installed_base": not base_current, "failed_skills": failed}))
finally:
    os.close(lock_fd)
