"""The bundled apply-patch skill: Codex-compatible V4A parsing and matching,
validate-first, in-place writes undone on failure, edit-style diff events."""

from __future__ import annotations

import asyncio
import builtins
import os
import sys
import tempfile
import types
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

SKILLS = Path(__file__).parents[2] / "skills"
sys.path.insert(0, str(SKILLS / "edit/src"))
sys.path.insert(0, str(SKILLS / "apply-patch/src"))

import apply_patch  # noqa: E402
import edit  # noqa: E402

_AS_ROOT = hasattr(os, "geteuid") and os.geteuid() == 0


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


def _patch(*lines: str) -> str:
    return "\n".join(("*** Begin Patch", *lines, "*** End Patch"))


class ApplyPatchSkillTest(unittest.TestCase):
    def setUp(self) -> None:
        self._previous_rlm = sys.modules.get("rlm")
        self.rlm = _RecordingRlm()
        self.rlm.install()
        self._cwd = os.getcwd()
        self._tmp = tempfile.TemporaryDirectory()
        os.chdir(self._tmp.name)
        self.root = Path(self._tmp.name).resolve()

    def tearDown(self) -> None:
        os.chdir(self._cwd)
        for directory, _, _ in os.walk(self._tmp.name):
            os.chmod(directory, 0o755)
        self._tmp.cleanup()
        if self._previous_rlm is None:
            sys.modules.pop("rlm", None)
        else:
            sys.modules["rlm"] = self._previous_rlm

    def write(self, name: str, content: str | bytes) -> Path:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(content, bytes):
            path.write_bytes(content)
        else:
            path.write_bytes(content.encode("utf-8"))
        return path

    def read(self, name: str) -> str:
        return (self.root / name).read_bytes().decode("utf-8")

    def tree(self) -> dict[str, str]:
        """Every file under the temp root (symlinks as `-> target`)."""
        found: dict[str, str] = {}
        for path in sorted(self.root.rglob("*")):
            name = str(path.relative_to(self.root))
            if path.is_symlink():
                found[name] = f"-> {os.readlink(path)}"
            elif path.is_file():
                found[name] = path.read_bytes().decode("utf-8", "replace")
        return found

    def apply(self, patch: str) -> list[dict[str, Any]]:
        return asyncio.run(apply_patch.run(patch))

    def diff_events(self) -> list[dict[str, Any]]:
        return [
            dict(
                event[edit._DIFF_DISPLAY_MIME],
                path=Path(event[edit._DIFF_DISPLAY_MIME]["path"]).name,
            )
            for event in self.rlm.events
        ]

    def assert_patch_error(self, patch: str, reason: str, message: str) -> None:
        with self.assertRaises(apply_patch.PatchError) as raised:
            self.apply(patch)
        self.assertEqual((raised.exception.reason, str(raised.exception)), (reason, message))

    def assert_untouched(self, patch: str, reason: str, message: str) -> None:
        """The patch fails with exactly this error and changes nothing."""
        before, events = self.tree(), len(self.rlm.events)
        self.assert_patch_error(patch, reason, message)
        self.assertEqual(self.tree(), before)
        self.assertEqual(len(self.rlm.events), events)

    # -- parsing (Codex parser.rs / streaming_parser.rs) ---------------------

    def test_update_with_anchor_applies_and_summarizes(self) -> None:
        self.write("src/app.py", 'def main():\n    print("hi")\n\nmain()\n')
        summary = self.apply(
            _patch(
                "*** Update File: src/app.py",
                "@@ def main():",
                '-    print("hi")',
                '+    print("hello")',
            )
        )
        self.assertEqual(self.read("src/app.py"), 'def main():\n    print("hello")\n\nmain()\n')
        self.assertEqual(
            summary,
            [{"path": "src/app.py", "operation": "update", "hunks": 1, "move_to": None}],
        )
        self.assertEqual(
            self.rlm.reports,
            [("apply_patch.report", {"files": 1, "hunks": 1, "failure": None})],
        )

    def test_malformed_patches_fail_with_codex_messages(self) -> None:
        self.write("a.txt", "a\n")
        not_a_header = (
            "is not a valid hunk header. Valid hunk headers: '*** Add File: {path}', "
            "'*** Delete File: {path}', '*** Update File: {path}'"
        )
        unexpected = (
            "Every line should start with ' ' (context line), '+' (added line), "
            "or '-' (removed line)"
        )
        cases = [
            (
                "*** Update File: a.txt\n-a\n+b\n*** End Patch",
                "Invalid patch: The first line of the patch must be '*** Begin Patch'",
            ),
            ("", "Invalid patch: The last line of the patch must be '*** End Patch'"),
            (
                "*** Begin Patch\n*** Update File: a.txt\n-a\n+b",
                "Invalid patch: The last line of the patch must be '*** End Patch'",
            ),
            (
                _patch("*** Add File: b.txt", "+b") + "\nextra",
                "Invalid patch: The last line of the patch must be '*** End Patch'",
            ),
            (_patch(), "No files were modified."),
            (_patch("hello"), f"Invalid patch hunk on line 2: 'hello' {not_a_header}"),
            (
                _patch("*** Update File:", "-a"),
                f"Invalid patch hunk on line 2: '*** Update File:' {not_a_header}",
            ),
            (
                _patch("*** Update File: a.txt"),
                "Invalid patch hunk on line 2: Update file hunk for path 'a.txt' is empty",
            ),
            (
                _patch("*** Update File: a.txt", "@@"),
                "Invalid patch hunk on line 4: Update hunk does not contain any lines",
            ),
            (
                _patch("*** Update File: a.txt", "@@", "*** End of File"),
                "Invalid patch hunk on line 4: Update hunk does not contain any lines",
            ),
            (
                _patch("*** Update File: a.txt", "@@", "?a"),
                f"Invalid patch hunk on line 4: Unexpected line found in update hunk: '?a'. {unexpected}",
            ),
            (
                _patch("*** Update File: a.txt", "@@", "-a", "bad"),
                "Invalid patch hunk on line 5: Expected update hunk to start with a @@ "
                "context marker, got: 'bad'",
            ),
            (
                _patch("*** Update File: a.txt", "@@", "-a", "*** End of File", "+b"),
                "Invalid patch hunk on line 6: Expected update hunk to start with a @@ "
                "context marker, got: '+b'",
            ),
            (
                _patch("*** Update File: a.txt", "@@", "*** Update File: b.txt"),
                "Invalid patch hunk on line 4: Unexpected line found in update hunk: "
                f"'*** Update File: b.txt'. {unexpected}",
            ),
            (
                _patch("*** Add File: b.txt", "b"),
                f"Invalid patch hunk on line 3: 'b' {not_a_header}",
            ),
            (
                _patch("*** Delete File: a.txt", "x"),
                f"Invalid patch hunk on line 3: 'x' {not_a_header}",
            ),
            # Codex: every Add line starts with '+'; a blank line is not a header.
            (
                _patch("*** Add File: b.txt", "+one", "", "+two"),
                f"Invalid patch hunk on line 4: '' {not_a_header}",
            ),
            # Consecutive `@@` anchors are rejected, as in Codex.
            (
                _patch("*** Update File: a.txt", "@@ class A:", "@@     def f(self):", "-a"),
                "Invalid patch hunk on line 4: Unexpected line found in update hunk: "
                f"'@@     def f(self):'. {unexpected}",
            ),
            (
                _patch("*** Update File: a.txt", "*** Move to: new.txt"),
                "Invalid patch hunk on line 2: Update file hunk for path 'a.txt' is empty",
            ),
        ]
        for patch, message in cases:
            with self.subTest(message=message):
                self.assert_untouched(patch, "parse", message)

    def test_envelope_whitespace_heredoc_and_crlf_are_tolerated(self) -> None:
        patches = [
            # Whitespace around the markers and the whole patch.
            "\n  *** Begin Patch  \n*** Add File: f.txt\n+v\n  *** End Patch \n\n",
            # Heredoc-wrapped (Codex lenient mode).
            "<<'EOF'\n*** Begin Patch\n*** Add File: f.txt\n+v\n*** End Patch\nEOF\n",
            '<<"EOF"\n*** Begin Patch\n*** Add File: f.txt\n+v\n*** End Patch\nEOF',
            # CRLF patch text.
            "*** Begin Patch\r\n*** Add File: f.txt\r\n+v\r\n*** End Patch\r\n",
            # Blank lines after the end marker.
            "*** Begin Patch\n*** Add File: f.txt\n+v\n*** End Patch\n \t\n",
        ]
        for patch in patches:
            with self.subTest(patch=patch):
                self.apply(patch)
                self.assertEqual(self.read("f.txt"), "v\n")
                os.unlink("f.txt")
        self.assert_untouched(
            "<<\"EOF'\n*** Begin Patch\n*** Add File: f.txt\n+v\n*** End Patch\nEOF",
            "parse",
            "Invalid patch: The first line of the patch must be '*** Begin Patch'",
        )

    def test_crlf_patch_updates_keep_the_file_line_endings(self) -> None:
        self.write("win.txt", "one\r\ntwo\r\nthree\r\n")
        self.apply(
            "*** Begin Patch\r\n*** Update File: win.txt\r\n@@\r\n one\r\n-two\r\n+TWO\r\n"
            "+2.5\r\n*** End Patch\r\n"
        )
        self.assertEqual(self.read("win.txt"), "one\r\nTWO\r\n2.5\r\nthree\r\n")

    def test_indented_headers_outside_updates_are_headers(self) -> None:
        self.write("a.txt", "a\n")
        self.write("gone.txt", "x\n")
        self.apply(
            "*** Begin Patch\n"
            "  *** Add File: added.txt\n"
            "+new\n"
            "\t*** Delete File: gone.txt\n"
            " *** Update File: a.txt\n"
            "@@\n"
            "-a\n"
            "+b\n"
            " *** End Patch"
        )
        self.assertEqual(self.tree(), {"a.txt": "b\n", "added.txt": "new\n"})

    def test_an_indented_header_inside_an_update_is_a_context_line(self) -> None:
        # Codex test_streaming_patch_parser_keeps_indented_update_markers_as_context_lines.
        self.write("a.txt", "old a\n*** Update File: b.txt\nold b\n")
        self.apply(
            _patch(
                "*** Update File: a.txt",
                "@@",
                "-old a",
                "+new a",
                " *** Update File: b.txt",
                "@@",
                "-old b",
                "+new b",
            )
        )
        self.assertEqual(self.read("a.txt"), "new a\n*** Update File: b.txt\nnew b\n")
        self.assertFalse((self.root / "b.txt").exists())

    def test_a_context_only_hunk_is_a_no_op(self) -> None:
        self.write("a.txt", "a\n")
        summary = self.apply(_patch("*** Update File: a.txt", "@@", " a"))
        self.assertEqual(self.read("a.txt"), "a\n")
        self.assertEqual(self.rlm.events, [])
        self.assertEqual(
            summary, [{"path": "a.txt", "operation": "update", "hunks": 1, "move_to": None}]
        )

    # -- matching (Codex seek_sequence.rs / file_update.rs) -------------------

    def test_updates_match_like_codex(self) -> None:
        cases = [
            # Hunk 2 is searched after hunk 1 in the original file, so the
            # marker hunk 1 inserts is not the one hunk 2 changes.
            (
                "start\nmarker\nend\n",
                ["@@", " start", "+marker", "+inserted", "@@", "-marker", "+MARKER"],
                "start\nmarker\ninserted\nMARKER\nend\n",
            ),
            # An anchor selects the occurrence after it.
            (
                "x = 1\nprint(x)\ny = 2\nprint(x)\n",
                ["@@ y = 2", "-print(x)", "+print(y)"],
                "x = 1\nprint(x)\ny = 2\nprint(y)\n",
            ),
            # Whitespace-tolerant context keeps the file's text.
            (
                "if x:   \n    y = 1\n",
                ["@@", " if x:", "-    y = 1", "+    y = 2"],
                "if x:   \n    y = 2\n",
            ),
            # Codex test_update_line_with_unicode_dash.
            (
                "import asyncio  # local import \u2013 avoids top\u2011level dep\n",
                [
                    "@@",
                    "-import asyncio  # local import - avoids top-level dep",
                    "+import asyncio  # HELLO",
                ],
                "import asyncio  # HELLO\n",
            ),
            ("x\ny\nx\n", ["@@", "-x", "+X", "*** End of File"], "x\ny\nX\n"),
            # Codex test_update_file_hunk_interleaved_changes.
            (
                "a\nb\nc\nd\ne\nf\n",
                [
                    "@@",
                    " a",
                    "-b",
                    "+B",
                    "@@",
                    " c",
                    " d",
                    "-e",
                    "+E",
                    "@@",
                    " f",
                    "+g",
                    "*** End of File",
                ],
                "a\nB\nc\nd\nE\nf\ng\n",
            ),
            # Codex test_pure_addition_chunk_followed_by_removal: a pure
            # addition goes to the end of the file.
            (
                "line1\nline2\nline3\n",
                [
                    "@@",
                    "+after-context",
                    "+second-line",
                    "@@",
                    " line1",
                    "-line2",
                    "-line3",
                    "+line2-replacement",
                ],
                "line1\nline2-replacement\nafter-context\nsecond-line\n",
            ),
            # Even after an anchor, and a file without a final newline gains one.
            ("a\nb", ["@@ a", "+appended"], "a\nb\nappended\n"),
            # A blank line after the header, and bare blank lines in a hunk,
            # are empty context (Codex
            # test_streaming_patch_parser_preserves_bare_empty_update_lines).
            ("a\nb\n", ["", "@@", "-b", "+B"], "a\nB\n"),
            (
                "context before\n\ncontext after\nold\n",
                ["@@", " context before", "", " context after", "-old", "+new"],
                "context before\n\ncontext after\nnew\n",
            ),
            # A pure addition goes before a final empty line, and a final
            # empty line left after the hunks is the file's last newline.
            ("a\n\n", ["@@", "+new"], "a\nnew\n"),
            ("a\n\n\n", ["@@", "+new"], "a\n\nnew\n"),
            ("a\n\n", ["@@", "-a", "+b"], "b\n"),
            ("a\nb\n", ["@@", " a", "-b", "+b", "+"], "a\nb\n"),
        ]
        for original, body, expected in cases:
            with self.subTest(original=original, body=body):
                self.write("f.txt", original)
                self.apply(_patch("*** Update File: f.txt", *body))
                self.assertEqual(self.read("f.txt"), expected)

    def test_a_missing_anchor_names_it(self) -> None:
        self.write("a.py", "a\n")
        self.assert_untouched(
            _patch("*** Update File: a.py", "@@ def nope():", "-a", "+b"),
            "context_not_found",
            "Failed to find context 'def nope():' in a.py (hunk 1)",
        )

    def test_end_of_file_hunks_only_match_at_the_end(self) -> None:
        self.write("head.txt", "foo\nbar\nbaz\n")
        self.assert_untouched(
            _patch("*** Update File: head.txt", "@@", "-foo", "+FOO", "*** End of File"),
            "context_not_found",
            "Failed to find expected lines in head.txt (hunk 1):\nfoo",
        )

    def test_a_patch_and_its_hand_written_edit_calls_are_equivalent(self) -> None:
        original = "def f():\n    return 1\n\n\ndef g():\n    return 2\n"
        self.write("f.py", original)
        self.apply(
            _patch(
                "*** Update File: f.py",
                "@@ def f():",
                "-    return 1",
                "+    return 10",
                "+    # ten",
                "@@ def g():",
                "-    return 2",
                "+    return 20",
            )
        )
        patched, patch_events = self.read("f.py"), list(self.rlm.events)
        self.rlm.events.clear()
        self.write("f.py", original)
        asyncio.run(edit.run("f.py", "    return 1\n", "    return 10\n    # ten\n"))
        asyncio.run(edit.run("f.py", "    return 2\n", "    return 20\n"))
        # Whole events, paths included: both name the file the same way.
        self.assertEqual((patched, patch_events), (self.read("f.py"), self.rlm.events))
        self.assertEqual(
            [event[edit._DIFF_DISPLAY_MIME]["path"] for event in patch_events],
            [str(self.root / "f.py")] * 2,
        )

    # -- Add / Delete / Move (Codex lib.rs apply_hunks_to_files) --------------

    def test_add_delete_move_and_summary(self) -> None:
        self.write("old.txt", "keep\n")
        self.write("doomed.txt", "bye\n")
        summary = self.apply(
            _patch(
                "*** Add File: pkg/new.txt",
                "+hello",
                "+world",
                "*** Delete File: doomed.txt",
                "*** Update File: old.txt",
                "*** Move to: moved/new_name.txt",
                "@@",
                "-keep",
                "+kept",
            )
        )
        self.assertEqual(
            self.tree(), {"moved/new_name.txt": "kept\n", "pkg/new.txt": "hello\nworld\n"}
        )
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
            ],
        )
        self.assertEqual(
            self.diff_events(),
            [
                {"path": "new.txt", "old_str": "", "new_str": "hello\nworld\n", "start_line": 1},
                {"path": "new_name.txt", "old_str": "keep\n", "new_str": "kept\n", "start_line": 1},
            ],
        )

    def test_add_replaces_an_existing_file(self) -> None:
        self.write("there.txt", "x\n")
        self.apply(_patch("*** Add File: there.txt", "+y"))
        self.assertEqual(self.read("there.txt"), "y\n")
        self.assertEqual(
            self.diff_events(),
            [{"path": "there.txt", "old_str": "x\n", "new_str": "y\n", "start_line": 1}],
        )

    def test_replacing_paths(self) -> None:
        cases = [
            # A move onto an existing file replaces it.
            (
                {"src.txt": "line\n", "dst.txt": "old destination\n"},
                ["*** Update File: src.txt", "*** Move to: dst.txt", "@@", "-line", "+line2"],
                {"dst.txt": "line2\n"},
            ),
            (
                {"same.txt": "before\n"},
                ["*** Delete File: same.txt", "*** Add File: same.txt", "+after"],
                {"same.txt": "after\n"},
            ),
            # A deleted file can become a directory.
            (
                {"notes": "flat\n"},
                ["*** Delete File: notes", "*** Add File: notes/today.txt", "+y"],
                {"notes/today.txt": "y\n"},
            ),
        ]
        for files, body, expected in cases:
            with self.subTest(body=body):
                for name, content in files.items():
                    self.write(name, content)
                self.apply(_patch(*body))
                self.assertEqual(self.tree(), expected)
                for name in expected:
                    (self.root / name).unlink()

    def test_missing_files_fail_validation(self) -> None:
        self.write("a.txt", "a\n")
        self.assert_untouched(
            _patch(
                "*** Update File: a.txt",
                "@@",
                "-a",
                "+b",
                "*** Update File: gone.txt",
                "@@",
                "-x",
                "+y",
            ),
            "file_missing",
            "Failed to read file to update gone.txt: No such file or directory",
        )
        self.assert_untouched(
            _patch("*** Delete File: gone.txt"),
            "file_missing",
            "Failed to delete file gone.txt: No such file or directory",
        )
        self.assert_untouched(
            _patch("*** Delete File: a.txt", "*** Update File: a.txt", "@@", "-a", "+b"),
            "file_missing",
            "Failed to read file to update a.txt: No such file or directory",
        )
        self.assertEqual(
            [payload["failure"] for _, payload in self.rlm.reports],
            ["file_missing", "file_missing", "file_missing"],
        )

    def test_a_failing_second_file_leaves_the_first_untouched(self) -> None:
        self.write("first.txt", "one\ntwo\n")
        self.write("second.txt", "alpha\n")
        self.assert_untouched(
            _patch(
                "*** Update File: first.txt",
                "@@",
                "-one",
                "+ONE",
                "*** Add File: added.txt",
                "+new",
                "*** Update File: second.txt",
                "@@",
                "-beta",
                "+BETA",
            ),
            "context_not_found",
            "Failed to find expected lines in second.txt (hunk 1):\nbeta",
        )
        self.assertEqual(
            self.rlm.reports,
            [("apply_patch.report", {"files": 0, "hunks": 0, "failure": "context_not_found"})],
        )

    # -- validate-first: nothing written when any write would fail ------------

    def test_a_file_where_a_directory_is_needed_fails_validation(self) -> None:
        self.write("src/a.py", "a\n")
        self.write("README.md", "readme\n")
        self.assert_untouched(
            _patch(
                "*** Update File: src/a.py",
                "@@",
                "-a",
                "+b",
                "*** Add File: README.md/notes.txt",
                "+notes",
            ),
            "path_conflict",
            "Failed to create parent directories for README.md/notes.txt: "
            f"{self.root / 'README.md'} is not a directory",
        )
        # A file this patch adds blocks a directory just the same.
        self.assert_untouched(
            _patch("*** Add File: notes", "+x", "*** Add File: notes/today.txt", "+y"),
            "path_conflict",
            f"Failed to create parent directories for notes/today.txt: {self.root / 'notes'} is a file",
        )
        os.mkdir(self.root / "pkg")
        self.assert_untouched(
            _patch("*** Add File: pkg", "+x"),
            "path_conflict",
            "Failed to write file pkg: Is a directory",
        )
        self.assert_untouched(
            _patch("*** Delete File: pkg"),
            "path_conflict",
            "Failed to delete file pkg: path is a directory",
        )

    @unittest.skipIf(_AS_ROOT, "root bypasses permission bits")
    def test_unwritable_targets_fail_validation(self) -> None:
        self.write("src/a.py", "a\n")
        locked = self.write("locked.txt", "ro\n")
        locked.chmod(0o444)
        self.assert_untouched(
            _patch(
                "*** Update File: src/a.py",
                "@@",
                "-a",
                "+b",
                "*** Update File: locked.txt",
                "@@",
                "-ro",
                "+rw",
            ),
            "not_writable",
            "Failed to write file locked.txt: Permission denied",
        )
        self.assert_untouched(
            _patch(
                "*** Update File: src/a.py", "@@", "-a", "+b", "*** Add File: locked.txt", "+rw"
            ),
            "not_writable",
            "Failed to write file locked.txt: Permission denied",
        )
        self.write("frozen/inside.txt", "x\n")
        (self.root / "frozen").chmod(0o555)
        for body, path in [
            (["*** Add File: frozen/new.txt", "+x"], "frozen/new.txt"),
            (["*** Add File: frozen/deeper/new.txt", "+x"], "frozen/deeper/new.txt"),
            (["*** Delete File: frozen/inside.txt"], "frozen/inside.txt"),
            (
                ["*** Update File: frozen/inside.txt", "*** Move to: out.txt", "@@", "-x", "+y"],
                "frozen/inside.txt",
            ),
        ]:
            with self.subTest(path=path):
                self.assert_untouched(
                    _patch("*** Update File: src/a.py", "@@", "-a", "+b", *body),
                    "not_writable",
                    f"Failed to write file {path}: directory {self.root / 'frozen'} is not writable",
                )

    def _undo_patch(self) -> str:
        """Fixture files and a patch touching every kind of change: two
        rewrites, a delete, a move, and a new file in new directories."""
        self.write("first.txt", "one\n")
        self.write("doomed.txt", "bye\n")
        self.write("src.txt", "move me\n")
        self.write("second.txt", "two\n")
        return _patch(
            "*** Update File: first.txt",
            "@@",
            "-one",
            "+ONE",
            "*** Delete File: doomed.txt",
            "*** Update File: src.txt",
            "*** Move to: moved/src.txt",
            "@@",
            "-move me",
            "+moved",
            "*** Add File: fresh/dir/new.txt",
            "+new",
            "*** Update File: second.txt",
            "@@",
            "-two",
            "+TWO",
        )

    def assert_undone(self, before: dict[str, str], first_inode: int) -> None:
        """Every file as it was, the rewritten one still the same file, no
        new directories or helper files, nothing displayed."""
        self.assertEqual(self.tree(), before)
        self.assertEqual(
            sorted(os.listdir(self.root)), ["doomed.txt", "first.txt", "second.txt", "src.txt"]
        )
        self.assertEqual((self.root / "first.txt").stat().st_ino, first_inode)
        self.assertEqual(self.rlm.events, [])

    def test_a_write_failure_mid_commit_undoes_every_change(self) -> None:
        patch = self._undo_patch()
        before, first_inode = self.tree(), (self.root / "first.txt").stat().st_ino
        real_open = builtins.open

        class FailingWrites:
            """The second rewrite's handle: its first write lands one byte,
            then fails, after every other change has been made."""

            def __init__(self, handle: Any) -> None:
                self._handle = handle
                self.failed = False

            def write(self, data: bytes) -> int:
                if not self.failed:
                    self.failed = True
                    self._handle.write(data[:1])
                    raise OSError(28, "No space left on device")
                return self._handle.write(data)

            def __getattr__(self, name: str) -> Any:
                return getattr(self._handle, name)

        def failing_open(path: Any, mode: str = "r", *args: Any, **kwargs: Any) -> Any:
            handle = real_open(path, mode, *args, **kwargs)
            if mode == "r+b" and Path(path).name == "second.txt":
                return FailingWrites(handle)
            return handle

        with mock.patch("builtins.open", failing_open):
            with self.assertRaises(apply_patch.PatchError) as raised:
                self.apply(patch)
        self.assertEqual(
            (raised.exception.reason, str(raised.exception)),
            (
                "apply_failed",
                f"Failed to write {self.root / 'second.txt'}: [Errno 28] No space left on device; "
                "every change was undone",
            ),
        )
        self.assert_undone(before, first_inode)

    def test_an_interruption_mid_commit_undoes_every_change(self) -> None:
        for interruption in (KeyboardInterrupt(), RuntimeError("not an OSError")):
            with self.subTest(interruption=type(interruption).__name__):
                patch = self._undo_patch()
                before, first_inode = self.tree(), (self.root / "first.txt").stat().st_ino
                # Raised once the delete and the move's removal are done.
                with mock.patch("os.mkdir", side_effect=interruption):
                    with self.assertRaises(type(interruption)) as raised:
                        self.apply(patch)
                self.assertIs(raised.exception, interruption)
                self.assert_undone(before, first_inode)

    def test_without_ownership_apis_updates_and_undo_work(self) -> None:
        # Windows has no os.chown.
        with mock.patch.dict(os.__dict__):
            del os.__dict__["chown"]
            patch = self._undo_patch()
            before, first_inode = self.tree(), (self.root / "first.txt").stat().st_ino
            with mock.patch("os.mkdir", side_effect=OSError(13, "Permission denied")):
                with self.assertRaises(apply_patch.PatchError):
                    self.apply(patch)
            self.assert_undone(before, first_inode)
            self.apply(patch)
        self.assertEqual(
            self.tree(),
            {
                "first.txt": "ONE\n",
                "fresh/dir/new.txt": "new\n",
                "moved/src.txt": "moved\n",
                "second.txt": "TWO\n",
            },
        )

    def test_surrogates_are_rejected_before_anything_is_written(self) -> None:
        # A non-raw literal turns `\ud83d` into a lone surrogate.
        self.write("a.txt", "a\n")
        self.assert_untouched(
            _patch("*** Update File: a.txt", "@@", "-a", "+b", "*** Add File: s.txt", "+\ud83d"),
            "parse",
            "Invalid patch: the new contents of s.txt are not valid UTF-8 text: 'utf-8' codec "
            "can't encode character '\\ud83d' in position 0: surrogates not allowed",
        )

    @unittest.skipIf(_AS_ROOT, "root bypasses permission bits")
    def test_rewrites_keep_the_file_and_its_hardlinks(self) -> None:
        original = self.write("a.txt", "a\n")
        original.chmod(0o640)
        os.link(original, self.root / "b.txt")
        inode = original.stat().st_ino
        self.apply(_patch("*** Update File: a.txt", "@@", "-a", "+A"))
        self.apply(_patch("*** Add File: b.txt", "+B"))
        status = original.stat()
        self.assertEqual(
            (status.st_ino, status.st_nlink, status.st_mode & 0o777, self.tree()),
            (inode, 2, 0o640, {"a.txt": "B\n", "b.txt": "B\n"}),
        )

    @unittest.skipIf(_AS_ROOT, "root bypasses permission bits")
    def test_a_writable_file_in_a_read_only_directory_can_be_rewritten(self) -> None:
        self.write("frozen/inside.txt", "x\n")
        self.write("frozen/other.txt", "o\n")
        (self.root / "frozen").chmod(0o555)
        self.apply(
            _patch(
                "*** Update File: frozen/inside.txt",
                "@@",
                "-x",
                "+y",
                "*** Add File: frozen/other.txt",
                "+replaced",
            )
        )
        self.assertEqual(
            self.tree(), {"frozen/inside.txt": "y\n", "frozen/other.txt": "replaced\n"}
        )

    @unittest.skipIf(_AS_ROOT, "root bypasses permission bits")
    def test_replaced_files_keep_their_mode(self) -> None:
        script = self.write("run.sh", "echo hi\n")
        script.chmod(0o751)
        self.apply(_patch("*** Update File: run.sh", "@@", "-echo hi", "+echo hello"))
        self.assertEqual(
            (script.stat().st_mode & 0o777, self.read("run.sh")), (0o751, "echo hello\n")
        )

    # -- binary files and symlinks --------------------------------------------

    def test_binary_files_can_be_deleted_and_replaced_but_not_updated(self) -> None:
        self.write("blob.bin", b"\xff\xfe\x00")
        self.write("other.bin", b"\x80")
        self.apply(_patch("*** Delete File: blob.bin", "*** Add File: other.bin", "+text"))
        self.assertEqual(self.tree(), {"other.bin": "text\n"})
        self.write("blob.bin", b"\xff\xfe\x00")
        before = self.tree()
        with self.assertRaises(apply_patch.PatchError) as raised:
            self.apply(_patch("*** Update File: blob.bin", "@@", "-x", "+y"))
        self.assertEqual(raised.exception.reason, "unreadable")
        self.assertTrue(
            str(raised.exception).startswith("Failed to read file to update blob.bin: ")
        )
        self.assertEqual(self.tree(), before)

    def test_updates_follow_links_while_deletes_and_moves_take_the_link(self) -> None:
        cases = [
            (
                ["*** Update File: link.txt", "@@", "-real", "+through link"],
                {"link.txt": "-> real.txt", "real.txt": "through link\n"},
            ),
            (["*** Delete File: link.txt"], {"real.txt": "real\n"}),
            # The target is still in play after its link is deleted: it can
            # be updated, and an Add of it replaces it (Codex Add semantics).
            (
                [
                    "*** Delete File: link.txt",
                    "*** Update File: real.txt",
                    "@@",
                    "-real",
                    "+updated",
                ],
                {"real.txt": "updated\n"},
            ),
            (
                ["*** Delete File: link.txt", "*** Add File: real.txt", "+added"],
                {"real.txt": "added\n"},
            ),
            (
                ["*** Update File: link.txt", "*** Move to: moved.txt", "@@", "-real", "+moved"],
                {"moved.txt": "moved\n", "real.txt": "real\n"},
            ),
        ]
        for body, expected in cases:
            with self.subTest(body=body):
                for path in self.root.iterdir():
                    path.unlink()
                self.write("real.txt", "real\n")
                os.symlink("real.txt", self.root / "link.txt")
                self.apply(_patch(*body))
                self.assertEqual(self.tree(), expected)

    def test_a_link_to_a_directory_cannot_be_deleted(self) -> None:
        os.mkdir(self.root / "dir")
        os.symlink("dir", self.root / "dirlink")
        self.assert_untouched(
            _patch("*** Delete File: dirlink"),
            "path_conflict",
            "Failed to delete file dirlink: path is a directory",
        )


if __name__ == "__main__":
    unittest.main()
