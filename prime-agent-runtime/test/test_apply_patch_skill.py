"""The bundled apply-patch skill: V4A parsing, validate-first, apply via edit."""

from __future__ import annotations

import asyncio
import os
import sys
import tempfile
import types
import unittest
from pathlib import Path
from typing import Any

SKILLS = Path(__file__).parents[2] / "skills"
sys.path.insert(0, str(SKILLS / "edit/src"))
sys.path.insert(0, str(SKILLS / "apply-patch/src"))

import apply_patch  # noqa: E402
import edit  # noqa: E402


class _RecordingRlm:
    """Stands in for the kernel's `rlm` module: records display events and
    host requests, so diff events and telemetry reports are observable."""

    def __init__(self) -> None:
        self.events: list[dict[str, Any]] = []
        self.reports: list[tuple[str, dict[str, Any]]] = []

    def install(self) -> None:
        module = types.ModuleType("rlm")
        module.emit = self.events.append  # type: ignore[attr-defined]

        async def host_request(request_type: str, payload: dict[str, Any]) -> dict[str, Any]:
            self.reports.append((request_type, payload))
            return {}

        module.host_request = host_request  # type: ignore[attr-defined]
        sys.modules["rlm"] = module


class ApplyPatchSkillTest(unittest.TestCase):
    def setUp(self) -> None:
        self._previous_rlm = sys.modules.get("rlm")
        self.rlm = _RecordingRlm()
        self.rlm.install()
        self._cwd = os.getcwd()
        self._tmp = tempfile.TemporaryDirectory()
        os.chdir(self._tmp.name)
        self.root = Path(self._tmp.name)

    def tearDown(self) -> None:
        os.chdir(self._cwd)
        self._tmp.cleanup()
        if self._previous_rlm is None:
            sys.modules.pop("rlm", None)
        else:
            sys.modules["rlm"] = self._previous_rlm

    def write(self, name: str, content: str) -> Path:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return path

    def apply(self, patch: str) -> list[dict[str, Any]]:
        return asyncio.run(apply_patch.run(patch))

    def assert_patch_error(self, patch: str, reason: str, message: str) -> None:
        with self.assertRaises(apply_patch.PatchError) as raised:
            self.apply(patch)
        self.assertEqual(raised.exception.reason, reason)
        self.assertIn(message, str(raised.exception))

    def test_update_with_anchor_applies_and_summarizes(self) -> None:
        app = self.write("src/app.py", 'def main():\n    print("hi")\n\nmain()\n')
        summary = self.apply(
            '*** Begin Patch\n'
            '*** Update File: src/app.py\n'
            '@@ def main():\n'
            '-    print("hi")\n'
            '+    print("hello")\n'
            '*** End Patch'
        )
        self.assertEqual(app.read_text(), 'def main():\n    print("hello")\n\nmain()\n')
        self.assertEqual(
            summary,
            [{"path": "src/app.py", "operation": "update", "hunks": 1, "move_to": None}],
        )
        self.assertEqual(
            self.rlm.reports,
            [("apply_patch.report", {"files": 1, "hunks": 1, "failure": None})],
        )

    def test_parser_rejects_malformed_envelopes(self) -> None:
        self.write("a.txt", "a\n")
        cases = [
            ("*** Update File: a.txt\n-a\n+b\n*** End Patch", "must start with"),
            ("*** Begin Patch\n*** Update File: a.txt\n-a\n+b", "must end with"),
            ("*** Begin Patch\n*** End Patch", "no file operations"),
            ("*** Begin Patch\nhello\n*** End Patch", "expected a file header"),
            ("*** Begin Patch\n*** Update File:\n-a\n*** End Patch", "needs a path"),
            ("*** Begin Patch\n*** Update File: a.txt\n*** End Patch", "no hunks"),
            ("*** Begin Patch\n*** Update File: a.txt\n@@\n a\n*** End Patch", "no '-' or '+'"),
            ("*** Begin Patch\n*** Update File: a.txt\n@@\n?a\n*** End Patch", "unexpected line"),
            ("*** Begin Patch\n*** Add File: b.txt\nb\n*** End Patch", "must start with '+'"),
        ]
        for patch, message in cases:
            with self.subTest(message=message):
                self.assert_patch_error(patch, "parse", message)
        self.assertEqual((self.root / "a.txt").read_text(), "a\n")
        self.assertFalse((self.root / "b.txt").exists())

    def test_a_failing_second_file_leaves_the_first_untouched(self) -> None:
        first = self.write("first.txt", "one\ntwo\n")
        second = self.write("second.txt", "alpha\n")
        self.assert_patch_error(
            '*** Begin Patch\n'
            '*** Update File: first.txt\n'
            '@@\n'
            '-one\n'
            '+ONE\n'
            '*** Add File: added.txt\n'
            '+new\n'
            '*** Update File: second.txt\n'
            '@@\n'
            '-beta\n'
            '+BETA\n'
            '*** End Patch',
            "context_not_found",
            "second.txt: hunk 1: context not found",
        )
        self.assertEqual(first.read_text(), "one\ntwo\n")
        self.assertEqual(second.read_text(), "alpha\n")
        self.assertFalse((self.root / "added.txt").exists())
        self.assertEqual(self.rlm.events, [])
        self.assertEqual(
            self.rlm.reports,
            [("apply_patch.report", {"files": 0, "hunks": 0, "failure": "context_not_found"})],
        )

    def test_missing_and_existing_files_fail_validation(self) -> None:
        self.write("there.txt", "x\n")
        self.assert_patch_error(
            "*** Begin Patch\n*** Update File: gone.txt\n@@\n-x\n+y\n*** End Patch",
            "file_missing",
            "gone.txt: file not found",
        )
        self.assert_patch_error(
            "*** Begin Patch\n*** Add File: there.txt\n+y\n*** End Patch",
            "file_exists",
            "there.txt: file already exists",
        )
        self.assert_patch_error(
            "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch",
            "file_missing",
            "gone.txt: file not found",
        )

    def test_hunks_apply_in_order_after_the_previous_one(self) -> None:
        # Hunk 1 introduces a line that hunk 2's context also matches; hunk 2
        # is searched after hunk 1, so it resolves to the original marker.
        path = self.write("order.txt", "start\nmarker\nend\n")
        self.apply(
            '*** Begin Patch\n'
            '*** Update File: order.txt\n'
            '@@\n'
            ' start\n'
            '+marker\n'
            '+inserted\n'
            '@@\n'
            '-marker\n'
            '+MARKER\n'
            '*** End Patch'
        )
        self.assertEqual(path.read_text(), "start\nmarker\ninserted\nMARKER\nend\n")

    def test_repeated_context_is_widened_until_edit_run_accepts_it(self) -> None:
        path = self.write("dup.py", "x = 1\nprint(x)\ny = 2\nprint(x)\n")
        self.apply(
            '*** Begin Patch\n'
            '*** Update File: dup.py\n'
            '@@ y = 2\n'
            '-print(x)\n'
            '+print(y)\n'
            '*** End Patch'
        )
        self.assertEqual(path.read_text(), "x = 1\nprint(x)\ny = 2\nprint(y)\n")

    def test_a_patch_and_its_hand_written_edit_calls_are_equivalent(self) -> None:
        original = "def f():\n    return 1\n\n\ndef g():\n    return 2\n"
        patched = self.write("eq_patch.py", original)
        self.apply(
            '*** Begin Patch\n'
            '*** Update File: eq_patch.py\n'
            '@@ def f():\n'
            '-    return 1\n'
            '+    return 10\n'
            '@@ def g():\n'
            '-    return 2\n'
            '+    return 20\n'
            '*** End Patch'
        )
        patch_events = [event[edit._DIFF_DISPLAY_MIME] for event in self.rlm.events]
        self.rlm.events.clear()
        by_hand = self.write("eq_hand.py", original)
        asyncio.run(edit.run("eq_hand.py", "    return 1\n", "    return 10\n"))
        asyncio.run(edit.run("eq_hand.py", "    return 2\n", "    return 20\n"))
        hand_events = [event[edit._DIFF_DISPLAY_MIME] for event in self.rlm.events]
        self.assertEqual(patched.read_text(), by_hand.read_text())
        # The two files differ only by name.
        self.assertEqual(
            [dict(event, path="f.py") for event in patch_events],
            [dict(event, path="f.py") for event in hand_events],
        )

    def test_add_delete_move_and_end_of_file(self) -> None:
        self.write("old.txt", "keep\n")
        self.write("doomed.txt", "bye\n")
        self.write("tail.txt", "a\nb\nlast")
        summary = self.apply(
            '*** Begin Patch\n'
            '*** Add File: pkg/new.txt\n'
            '+hello\n'
            '+world\n'
            '*** Delete File: doomed.txt\n'
            '*** Update File: old.txt\n'
            '*** Move to: moved/new_name.txt\n'
            '@@\n'
            '-keep\n'
            '+kept\n'
            '*** Update File: tail.txt\n'
            '@@\n'
            ' b\n'
            '-last\n'
            '+final\n'
            '*** End of File\n'
            '*** End Patch'
        )
        self.assertEqual((self.root / "pkg/new.txt").read_text(), "hello\nworld\n")
        self.assertFalse((self.root / "doomed.txt").exists())
        self.assertFalse((self.root / "old.txt").exists())
        self.assertEqual((self.root / "moved/new_name.txt").read_text(), "kept\n")
        self.assertEqual((self.root / "tail.txt").read_text(), "a\nb\nfinal")
        self.assertEqual(
            summary,
            [
                {"path": "pkg/new.txt", "operation": "add", "hunks": 0, "move_to": None},
                {"path": "doomed.txt", "operation": "delete", "hunks": 0, "move_to": None},
                {
                    "path": "old.txt",
                    "operation": "update",
                    "hunks": 1,
                    "move_to": "moved/new_name.txt",
                },
                {"path": "tail.txt", "operation": "update", "hunks": 1, "move_to": None},
            ],
        )
        added_event = self.rlm.events[0][edit._DIFF_DISPLAY_MIME]
        self.assertEqual(
            dict(added_event, path=Path(added_event["path"]).name),
            {"path": "new.txt", "old_str": "", "new_str": "hello\nworld\n", "start_line": 1},
        )

    def test_whitespace_tolerant_context_keeps_the_file_text(self) -> None:
        path = self.write("ws.py", "if x:   \n    y = 1\n")
        self.apply(
            '*** Begin Patch\n'
            '*** Update File: ws.py\n'
            '@@\n'
            ' if x:\n'
            '-    y = 1\n'
            '+    y = 2\n'
            '*** End Patch'
        )
        self.assertEqual(path.read_text(), "if x:   \n    y = 2\n")

    def test_pure_insertion_after_anchor_and_at_end_of_file(self) -> None:
        path = self.write("ins.txt", "a\nb")
        self.apply(
            '*** Begin Patch\n'
            '*** Update File: ins.txt\n'
            '@@ a\n'
            '+between\n'
            '*** Update File: ins.txt\n'
            '@@\n'
            '+appended\n'
            '*** End Patch'
        )
        self.assertEqual(path.read_text(), "a\nbetween\nb\nappended")


if __name__ == "__main__":
    unittest.main()
