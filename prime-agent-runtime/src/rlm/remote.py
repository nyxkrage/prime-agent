"""Remote (SSH) kernel concerns on the runtime side.

A kernel started by ``rlm.spawn(..., host=...)`` runs on another machine
while its agent loop stays on the orchestrator's. The host marks it with
``RLM_REMOTE_HOST``; this module then:

- refuses features whose state lives on the orchestrator's machine
  (``rlm.harness``, ``bash(persistent=True)``) with a clear error, never a
  silent split of state (``refuse_on_remote``);
- exits when the host goes silent: the host sends a heartbeat frame every
  few seconds, and a partitioned connection that sshd never notices would
  otherwise leave the kernel and its process groups running forever;
- journals its ``bash()`` process groups under the install root and reaps
  the groups of kernels that died without cleaning up (SIGKILL, OOM) on
  the next remote kernel start;
- holds a shared lock on its install dir so the installer's garbage
  collection never removes an install a kernel is running from.
"""

from __future__ import annotations

import atexit
import json
import os
import sys
import threading
import time
from collections.abc import Callable

REMOTE_HOST_ENV = "RLM_REMOTE_HOST"
INSTALL_DIR_ENV = "RLM_REMOTE_INSTALL_DIR"
HEARTBEAT_TIMEOUT_ENV = "RLM_HOST_HEARTBEAT_TIMEOUT_MS"

_last_host_frame = time.monotonic()
# The shared flock on the install's .inuse file, held for the kernel's life.
_in_use_fd: int | None = None


class RemoteKernelRefusal(RuntimeError):
    """A feature that is not available on remote kernels (yet)."""


def remote_host() -> str | None:
    """The SSH host this kernel runs on, or None for a local kernel."""
    host = (os.environ.get(REMOTE_HOST_ENV) or "").strip()
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


def _heartbeat_watchdog(timeout_s: float, on_silence: Callable[[], None]) -> None:
    while True:
        silent = time.monotonic() - _last_host_frame
        if silent >= timeout_s:
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


def _hold_install(install_dir: str) -> None:
    import fcntl  # POSIX only: remote kernels never run on Windows

    global _in_use_fd
    fd = os.open(os.path.join(install_dir, ".inuse"), os.O_RDWR | os.O_CREAT, 0o600)
    fcntl.flock(fd, fcntl.LOCK_SH)
    _in_use_fd = fd
    last_used = os.path.join(install_dir, ".last-used")
    with open(last_used, "a"):
        pass
    os.utime(last_used)


def configure(on_silence: Callable[[], None]) -> None:
    """Set up a remote kernel before the protocol starts; a no-op locally.

    ``on_silence`` runs on the watchdog thread when the host goes silent;
    it must end the process."""
    if remote_host() is None:
        return
    # The owner is the remote sshd session this kernel was exec'd from: the
    # owner watchdog exits when it goes away, and journal records name it.
    os.environ["PRIME_AGENT_KERNEL_OWNER_PID"] = str(os.getppid())
    install_dir = (os.environ.get(INSTALL_DIR_ENV) or "").strip()
    if install_dir:
        _hold_install(install_dir)
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
    raw = os.environ.get(HEARTBEAT_TIMEOUT_ENV, "")
    try:
        timeout_ms = int(raw)
    except ValueError:
        timeout_ms = 0
    if timeout_ms > 0:
        threading.Thread(
            target=_heartbeat_watchdog,
            args=(timeout_ms / 1000.0, on_silence),
            daemon=True,
            name="prime-agent-host-heartbeat",
        ).start()


__all__ = [
    "RemoteKernelRefusal",
    "configure",
    "note_host_frame",
    "reap_stale_kernels",
    "refuse_on_remote",
    "remote_host",
]
