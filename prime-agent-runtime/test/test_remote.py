"""Remote (SSH) kernel runtime concerns: the host heartbeat watchdog, the
refusals, and the stale remote kernel reaper (rlm/remote.py)."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
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


class RefusalTest(unittest.TestCase):
    def test_refusals_name_the_feature_host_and_reason_only_on_remote_kernels(self) -> None:
        saved = os.environ.pop(remote.REMOTE_HOST_ENV, None)
        self.addCleanup(lambda: os.environ.pop(remote.REMOTE_HOST_ENV, None))
        if saved is not None:
            self.addCleanup(os.environ.__setitem__, remote.REMOTE_HOST_ENV, saved)
        remote.refuse_on_remote("thing", "why")
        os.environ[remote.REMOTE_HOST_ENV] = "user@box"
        with self.assertRaises(remote.RemoteKernelRefusal) as raised:
            remote.refuse_on_remote("thing", "why")
        self.assertEqual(
            str(raised.exception),
            "thing is not available on remote kernels yet (this kernel runs on user@box): why",
        )


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
