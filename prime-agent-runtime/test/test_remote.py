"""Remote (SSH) kernel runtime concerns: the host heartbeat watchdog, the
refusals, and the stale remote kernel reaper (rlm/remote.py)."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

from test_repl import ReplProcess  # noqa: E402

from rlm import remote  # noqa: E402
from rlm.bash import _process_start_id  # noqa: E402


class HeartbeatTest(unittest.TestCase):
    def remote_repl(self, timeout_ms: int) -> ReplProcess:
        repl = ReplProcess(
            env={
                "RLM_REMOTE_HOST": "box",
                "RLM_HOST_HEARTBEAT_TIMEOUT_MS": str(timeout_ms),
            }
        )
        self.addCleanup(repl.close)
        repl.send({"type": "kernel_env", "env": {}})
        event, _ = repl.ready()
        self.assertEqual(event["event"], "ready")
        return repl

    def test_a_silent_host_ends_the_remote_kernel_and_its_bash_groups(self) -> None:
        repl = self.remote_repl(1500)
        events = repl.execute("c1", "from rlm import bash\nh = bash('sleep 600')\nprint(h.pid)")
        bash_pid = int("".join(e["text"] for e in events if e.get("event") == "stdout").strip())
        # Stdin stays open (a partition, not a closed connection): only the
        # missing heartbeat can end the kernel.
        self.assertEqual(repl.proc.wait(timeout=30), 1)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                os.killpg(bash_pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.05)
        else:
            self.fail("the bash group outlived the kernel")

    def test_heartbeats_keep_the_remote_kernel_alive(self) -> None:
        repl = self.remote_repl(1000)
        started = time.monotonic()
        while time.monotonic() - started < 3:
            repl.send({"type": "heartbeat"})
            time.sleep(0.2)
        self.assertIsNone(repl.proc.poll())
        events = repl.execute("c1", "print('alive')")
        self.assertIn("alive", "".join(e.get("text", "") for e in events))
        self.assertEqual(repl.shutdown(), 0)

    def test_a_local_kernel_ignores_heartbeats_and_has_no_watchdog(self) -> None:
        repl = ReplProcess()
        self.addCleanup(repl.close)
        repl.ready()
        repl.send({"type": "heartbeat"})
        events = repl.execute("c1", "print('local')")
        self.assertNotIn("error", [e.get("event") for e in events])
        self.assertEqual(repl.shutdown(), 0)


class KernelEnvTest(unittest.TestCase):
    """The ``kernel_env`` frame: a remote kernel's spawn env, read from stdin
    before ``ready`` and applied before any user code runs."""

    def test_a_remote_kernel_applies_the_env_frame_before_ready(self) -> None:
        repl = ReplProcess(env={"RLM_REMOTE_HOST": "box", "PA_TEST_ENV": "inherited"})
        self.addCleanup(repl.close)
        repl.send({"type": "kernel_env", "env": {"PA_TEST_ENV": "from-frame", "EPOCHS": "3"}})
        event, _ = repl.ready()
        self.assertEqual(event["event"], "ready")
        events = repl.execute("c1", "import os\nprint(os.environ['PA_TEST_ENV'], os.environ['EPOCHS'])")
        self.assertEqual("".join(e.get("text", "") for e in events if e.get("event") == "stdout"), "from-frame 3\n")
        self.assertEqual(repl.shutdown(), 0)

    def test_a_malformed_env_frame_exits_before_ready(self) -> None:
        repl = ReplProcess(env={"RLM_REMOTE_HOST": "box"})
        self.addCleanup(repl.close)
        repl.send({"type": "execute", "id": "c1", "code": "print(1)"})
        with self.assertRaises(EOFError):
            repl.ready()
        self.assertEqual(repl.proc.wait(timeout=10), 1)

    def test_a_local_kernel_reads_no_env_frame(self) -> None:
        repl = ReplProcess()
        self.addCleanup(repl.close)
        event, _ = repl.ready()
        self.assertEqual(event["event"], "ready")
        self.assertEqual(repl.shutdown(), 0)


class RefusalTest(unittest.TestCase):
    def test_refusals_name_the_feature_host_and_reason_only_on_remote_kernels(self) -> None:
        saved = dict(remote._markers)
        self.addCleanup(lambda: (remote._markers.clear(), remote._markers.update(saved)))
        remote._markers.clear()
        remote.refuse_on_remote("thing", "why")
        remote._markers[remote.REMOTE_HOST_ENV] = "user@box"
        with self.assertRaises(remote.RemoteKernelRefusal) as raised:
            remote.refuse_on_remote("thing", "why")
        self.assertEqual(
            str(raised.exception),
            "thing is not available on remote kernels yet (this kernel runs on user@box): why",
        )


class MarkerTest(unittest.TestCase):
    """The launcher's markers configure the runtime and then leave
    ``os.environ``: a child process (a ``bash`` command, a ``prime-agent``
    started from it) must not think its own kernels are remote."""

    def test_markers_stay_out_of_the_kernel_env_and_its_children(self) -> None:
        install = tempfile.mkdtemp()
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", install], check=False))
        markers = {
            "RLM_REMOTE_HOST": "box",
            "RLM_REMOTE_INSTALL_DIR": os.path.join(install, "v3-0000000000000000"),
            "RLM_HOST_HEARTBEAT_TIMEOUT_MS": "60000",
            "RLM_REMOTE_SKILLS": "edit-0123456789abcdef",
        }
        os.makedirs(markers["RLM_REMOTE_INSTALL_DIR"])
        repl = ReplProcess(env=markers)
        self.addCleanup(repl.close)
        repl.send({"type": "kernel_env", "env": {}})
        repl.ready()
        events = repl.execute(
            "c1",
            "import os, subprocess\n"
            "from rlm import remote\n"
            f"names = {sorted(markers)!r}\n"
            "print([n for n in names if n in os.environ])\n"
            "print(subprocess.run(['env'], capture_output=True, text=True).stdout.count('RLM_REMOTE'))\n"
            "print(remote.remote_host())",
        )
        stdout = "".join(e.get("text", "") for e in events if e.get("event") == "stdout")
        self.assertEqual(stdout, "[]\n0\nbox\n")
        self.assertEqual(repl.shutdown(), 0)


class WatchdogTest(unittest.TestCase):
    """The heartbeat watchdog against a stale timestamp: pending stdin (a
    reader starved by a GIL-holding C call) is not a lost host."""

    def setUp(self) -> None:
        saved = remote._last_host_frame
        self.addCleanup(lambda: setattr(remote, "_last_host_frame", saved))
        read_fd, write_fd = os.pipe()
        self.addCleanup(os.close, read_fd)
        self.addCleanup(os.close, write_fd)
        self.read_fd, self.write_fd = read_fd, write_fd
        self.silenced = threading.Event()

    def watch(self) -> None:
        threading.Thread(
            target=remote._heartbeat_watchdog,
            args=(3.0, self.read_fd, self.silenced.set),
            daemon=True,
        ).start()

    def test_pending_heartbeats_keep_a_stale_kernel_alive(self) -> None:
        os.write(self.write_fd, b'{"type": "heartbeat"}\n')
        remote._last_host_frame = time.monotonic() - 3600

        def reader_catches_up() -> None:
            time.sleep(0.5)
            os.read(self.read_fd, 4096)
            remote.note_host_frame()

        threading.Thread(target=reader_catches_up, daemon=True).start()
        self.watch()
        # Kept alive through the grace: the reader caught up after 0.5s.
        self.assertFalse(self.silenced.wait(remote._READER_GRACE_S + 0.5))
        # The silence still counts once the host really stops.
        self.assertTrue(self.silenced.wait(10))

    def test_a_stale_kernel_with_no_pending_input_exits(self) -> None:
        remote._last_host_frame = time.monotonic() - 3600
        self.watch()
        self.assertTrue(self.silenced.wait(remote._READER_GRACE_S / 2))


class ReapTest(unittest.TestCase):
    def test_stale_kernels_lose_their_groups_and_live_ones_are_kept(self) -> None:
        run_dir = tempfile.mkdtemp()
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", run_dir], check=False))
        # A "dead kernel" whose journal still lists a live process group.
        orphan = subprocess.Popen(["sleep", "600"], start_new_session=True)
        self.addCleanup(orphan.kill)
        dead = subprocess.Popen(["true"])
        dead.wait()
        with open(os.path.join(run_dir, f"{dead.pid}.kernel"), "w") as handle:
            json.dump({"pid": dead.pid, "startId": "proc:0"}, handle)
        with open(os.path.join(run_dir, f"{dead.pid}.journal"), "w") as handle:
            handle.write(
                json.dumps({"pid": orphan.pid, "active": True, "processStartId": _process_start_id(orphan.pid)})
                + "\n"
            )
        # A live kernel (this process) is never reaped.
        me = os.getpid()
        with open(os.path.join(run_dir, f"{me}.kernel"), "w") as handle:
            json.dump({"pid": me, "startId": _process_start_id(me)}, handle)

        self.assertEqual(remote.reap_stale_kernels(run_dir, own_pid=-1), [dead.pid])
        self.assertEqual(orphan.wait(timeout=10), -9)
        self.assertEqual(sorted(os.listdir(run_dir)), [f"{me}.kernel"])


if __name__ == "__main__":
    unittest.main()
