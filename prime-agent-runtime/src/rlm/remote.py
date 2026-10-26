"""Remote (SSH) kernel concerns on the runtime side.

The launcher marks a remote kernel with ``RLM_REMOTE_*`` env markers; this
module takes them out of ``os.environ`` before user code runs, so no child
process (a ``prime-agent`` started from ``bash``) mistakes its own kernels
for remote ones. It then:

- refuses features whose state lives on the orchestrator's machine
  (``refuse_on_remote``) instead of silently splitting state;
- exits when the host goes silent (a partition sshd never notices);
- journals its ``bash()`` process groups and reaps those of remote kernels
  that died without cleaning up;
- holds shared locks on its install and skill dirs against the installer's
  garbage collection, and puts its pinned skill sites on ``sys.path``;
- applies its ``rlm.spawn(env=...)`` values from the first stdin frame, so
  they never ride the ssh command line (visible in the remote ``ps``).
"""

from __future__ import annotations

import atexit
import json
import os
import select
import sys
import threading
import time
from collections.abc import Callable

REMOTE_HOST_ENV = "RLM_REMOTE_HOST"
INSTALL_DIR_ENV = "RLM_REMOTE_INSTALL_DIR"
HEARTBEAT_TIMEOUT_ENV = "RLM_HOST_HEARTBEAT_TIMEOUT_MS"
SKILLS_ENV = "RLM_REMOTE_SKILLS"
# Taken out of os.environ by begin(), never inherited by a child process.
_MARKER_ENV = (REMOTE_HOST_ENV, INSTALL_DIR_ENV, HEARTBEAT_TIMEOUT_ENV, SKILLS_ENV)
# How long the watchdog lets the reader thread catch up on pending stdin
# before it treats a stale heartbeat as a lost host.
_READER_GRACE_S = 2.0

KERNEL_ENV_FRAME = "kernel_env"
# Far above the host's 64 KiB payload cap, JSON escaping included.
_MAX_KERNEL_ENV_FRAME_BYTES = 1 << 20

_last_host_frame = time.monotonic()
# The shared flocks on the install's and the skill dirs' .inuse files, held
# for the kernel's life.
_in_use_fds: list[int] = []
# The launcher markers begin() took out of os.environ.
_markers: dict[str, str] = {}


class RemoteKernelRefusal(RuntimeError):
    """A feature that is not available on remote kernels (yet)."""


def remote_host() -> str | None:
    """The SSH host this kernel runs on, or None for a local kernel."""
    host = (_markers.get(REMOTE_HOST_ENV) or "").strip()
    return host or None


def refuse_on_remote(feature: str, reason: str) -> None:
    """Raise RemoteKernelRefusal naming ``feature`` when this kernel is remote."""
    host = remote_host()
    if host is not None:
        raise RemoteKernelRefusal(
            f"{feature} is not available on remote kernels yet (this kernel runs on {host}): {reason}"
        )


def note_host_frame() -> None:
    """Record that a protocol frame (any request, or a heartbeat) arrived."""
    global _last_host_frame
    _last_host_frame = time.monotonic()


def _heartbeat_watchdog(timeout_s: float, stdin_fd: int, on_silence: Callable[[], None]) -> None:
    while True:
        silent = time.monotonic() - _last_host_frame
        if silent >= timeout_s:
            # A C call that holds the GIL past the timeout starves the reader
            # thread while heartbeats wait unread in the pipe: pending input
            # means the host is alive, so the reader gets a moment to catch
            # up before the silence counts. (EOF also reads as pending; the
            # grace then just delays an exit the reader starts anyway.)
            if select.select([stdin_fd], [], [], 0)[0]:
                time.sleep(_READER_GRACE_S)
                if time.monotonic() - _last_host_frame < timeout_s:
                    continue
            silent = time.monotonic() - _last_host_frame
            print(
                f"prime-agent: no frame from the host for {silent:.0f}s; the connection is gone, exiting",
                file=sys.__stderr__,
                flush=True,
            )
            on_silence()
            return
        time.sleep(min(1.0, timeout_s - silent))


def _process_start_id(pid: int) -> str | None:
    from .bash import _process_start_id as start_id

    return start_id(pid)


def _run_dir(install_dir: str) -> str:
    return os.path.join(os.path.dirname(install_dir.rstrip("/")), "run")


def reap_stale_kernels(run_dir: str, own_pid: int) -> list[int]:
    """Kill the journaled process groups of remote kernels that died without
    cleaning up, and drop their records. A kernel is stale when its pid is
    gone or now belongs to another process (start identity changed).
    Returns the reaped kernel pids."""
    from .bash import _reap_persistent_group

    reaped: list[int] = []
    try:
        names = sorted(os.listdir(run_dir))
    except FileNotFoundError:
        return reaped
    for name in names:
        if not name.endswith(".kernel"):
            continue
        record_path = os.path.join(run_dir, name)
        try:
            with open(record_path) as handle:
                record = json.load(handle)
            pid = int(record["pid"])
        except (OSError, ValueError, KeyError, TypeError):
            continue
        if pid == own_pid:
            continue
        current = _process_start_id(pid)
        if current is not None and current == record.get("startId"):
            continue  # still running; its heartbeat watchdog bounds its life
        journal = os.path.join(run_dir, f"{pid}.journal")
        active: dict[int, str | None] = {}
        try:
            with open(journal) as handle:
                for line in handle:
                    try:
                        entry = json.loads(line)
                        group = int(entry["pid"])
                    except (ValueError, KeyError, TypeError):
                        continue
                    if entry.get("active"):
                        active[group] = entry.get("processStartId")
                    else:
                        active.pop(group, None)
        except FileNotFoundError:
            pass
        for group, start_id in active.items():
            if start_id:
                _reap_persistent_group(group, start_id)
        for path in (journal, record_path):
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
        reaped.append(pid)
    return reaped


def _hold(directory: str) -> None:
    """Hold a shared flock on ``directory``'s .inuse file for the kernel's
    life and touch its .last-used: the installer's garbage collection skips
    a dir whose .inuse it cannot lock exclusively."""
    import fcntl  # POSIX only: remote kernels never run on Windows

    fd = os.open(os.path.join(directory, ".inuse"), os.O_RDWR | os.O_CREAT, 0o600)
    fcntl.flock(fd, fcntl.LOCK_SH)
    _in_use_fds.append(fd)
    last_used = os.path.join(directory, ".last-used")
    with open(last_used, "a"):
        pass
    os.utime(last_used)


def read_kernel_env(stdin_fd: int) -> dict[str, str]:
    """Read the one ``kernel_env`` frame a remote kernel's host sends first.

    Byte by byte, so nothing past the frame's newline is consumed: the
    protocol reader takes the fd over afterwards."""
    line = bytearray()
    while True:
        byte = os.read(stdin_fd, 1)
        if not byte:
            raise RuntimeError("stdin closed before the host sent the kernel env")
        if byte == b"\n":
            break
        line += byte
        if len(line) > _MAX_KERNEL_ENV_FRAME_BYTES:
            raise RuntimeError(f"the kernel env frame exceeds {_MAX_KERNEL_ENV_FRAME_BYTES} bytes")
    frame = json.loads(line)
    if not isinstance(frame, dict) or frame.get("type") != KERNEL_ENV_FRAME:
        raise RuntimeError("the host's first frame is not the kernel env")
    env = frame.get("env")
    if not isinstance(env, dict) or not all(
        isinstance(key, str) and isinstance(value, str) for key, value in env.items()
    ):
        raise RuntimeError("the kernel env frame must map str names to str values")
    return env


def begin(stdin_fd: int) -> None:
    """Take the launcher's markers out of ``os.environ`` (this module keeps
    them), then apply a remote kernel's ``rlm.spawn(env=...)`` values from
    its first stdin frame. A local kernel reads no frame (it gets its spawn
    env in its process env)."""
    _markers.clear()
    _markers.update({key: os.environ.pop(key) for key in _MARKER_ENV if key in os.environ})
    if remote_host() is None:
        return
    os.environ.update(read_kernel_env(stdin_fd))


def configure(stdin_fd: int, on_silence: Callable[[], None]) -> None:
    """Set up a remote kernel before the protocol starts; a no-op locally.

    ``stdin_fd`` is the protocol input the heartbeats arrive on.
    ``on_silence`` runs on the watchdog thread when the host goes silent;
    it must end the process."""
    if remote_host() is None:
        return
    # The owner is the remote sshd session this kernel was exec'd from: the
    # owner watchdog exits when it goes away, and journal records name it.
    os.environ["PRIME_AGENT_KERNEL_OWNER_PID"] = str(os.getppid())
    install_dir = (_markers.get(INSTALL_DIR_ENV) or "").strip()
    if install_dir:
        _hold(install_dir)
        # ``<root>/skills/<id>/`` holds a skill's files and
        # ``<install>/skill-sites/<id>/`` its built Python package; ids are
        # content-hashed, so the sites pin this kernel's skill versions.
        root = os.path.dirname(install_dir.rstrip("/"))
        sites = []
        for skill_id in (_markers.get(SKILLS_ENV) or "").split():
            store = os.path.join(root, "skills", skill_id)
            if os.path.isdir(store):
                _hold(store)
            site = os.path.join(install_dir, "skill-sites", skill_id)
            if os.path.isdir(site):
                _hold(site)
                sites.append(site)
        position = next(
            (index for index, entry in enumerate(sys.path) if entry.rstrip("/").endswith("site-packages")),
            len(sys.path),
        )
        sys.path[position:position] = sites
        run_dir = _run_dir(install_dir)
        os.makedirs(run_dir, exist_ok=True)
        pid = os.getpid()
        reap_stale_kernels(run_dir, pid)
        record = os.path.join(run_dir, f"{pid}.kernel")
        journal = os.path.join(run_dir, f"{pid}.journal")
        with open(record, "w") as handle:
            json.dump({"pid": pid, "startId": _process_start_id(pid)}, handle)
        os.environ["PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL"] = journal

        def forget() -> None:
            for path in (journal, record):
                try:
                    os.unlink(path)
                except OSError:
                    pass

        atexit.register(forget)
    raw = _markers.get(HEARTBEAT_TIMEOUT_ENV, "")
    try:
        timeout_ms = int(raw)
    except ValueError:
        timeout_ms = 0
    if timeout_ms > 0:
        threading.Thread(
            target=_heartbeat_watchdog,
            args=(timeout_ms / 1000.0, stdin_fd, on_silence),
            daemon=True,
            name="prime-agent-host-heartbeat",
        ).start()


__all__ = [
    "RemoteKernelRefusal",
    "begin",
    "configure",
    "note_host_frame",
    "read_kernel_env",
    "reap_stale_kernels",
    "refuse_on_remote",
    "remote_host",
]
