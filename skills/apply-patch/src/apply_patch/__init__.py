"""Apply OpenAI V4A patches: validate the whole patch in memory, then write
every update hunk through the ``edit`` skill.

Phase 1 parses the patch and resolves every hunk into an exact
``old_str -> new_str`` pair, replaying the pairs in memory with the rule
``edit.run`` enforces (``old_str`` occurs exactly once in the current
content). Any failure raises :class:`PatchError` and nothing is written.
Phase 2 calls ``edit.run`` once per pair in the validated order, so file
writes and diff display events are exactly what direct ``edit`` calls
produce. Added files are written here (with a diff display event); deleted
and moved files are unlinked/renamed here.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import edit

_BEGIN = "*** Begin Patch"
_END = "*** End Patch"
_ADD = "*** Add File:"
_DELETE = "*** Delete File:"
_UPDATE = "*** Update File:"
_MOVE = "*** Move to:"
_END_OF_FILE = "*** End of File"

# Host request carrying the adoption counters (no paths, no content).
_REPORT_REQUEST = "apply_patch.report"


class PatchError(ValueError):
    """A patch that failed to parse, validate, or apply.

    ``reason`` is one of ``parse``, ``context_not_found``, ``file_missing``,
    ``file_exists``, ``apply_failed`` (keep in sync with
    ``APPLY_PATCH_FAILURE_REASONS`` in ``crates/pa-telemetry/src/catalog.rs``).
    Only ``apply_failed`` can leave files partially changed (a file changed
    on disk between validation and write).
    """

    def __init__(self, message: str, *, reason: str) -> None:
        super().__init__(message)
        self.reason = reason


@dataclass
class _Hunk:
    anchors: list[str] = field(default_factory=list)
    lines: list[tuple[str, str]] = field(default_factory=list)
    at_eof: bool = False


@dataclass
class _Operation:
    kind: str  # "update" | "add" | "delete"
    path: str
    move_to: str | None = None
    hunks: list[_Hunk] = field(default_factory=list)
    added: str = ""


def _parse(patch: str) -> list[_Operation]:
    lines = patch.strip().split("\n")
    lines = [line[:-1] if line.endswith("\r") else line for line in lines]
    if lines[0].strip() != _BEGIN:
        raise PatchError(f"patch must start with {_BEGIN!r}", reason="parse")
    if len(lines) < 2 or lines[-1].strip() != _END:
        raise PatchError(f"patch must end with {_END!r}", reason="parse")
    body = lines[1:-1]
    operations: list[_Operation] = []
    index = 0

    def header_path(line: str, header: str) -> str:
        path = line[len(header) :].strip()
        if not path:
            raise PatchError(f"line {index + 2}: {header!r} needs a path", reason="parse")
        return path

    while index < len(body):
        line = body[index]
        if not line.strip():
            index += 1
            continue
        if line.startswith(_ADD):
            operation = _Operation("add", header_path(line, _ADD))
            index += 1
            added: list[str] = []
            while index < len(body) and not body[index].startswith("*** "):
                if not body[index].startswith("+"):
                    raise PatchError(
                        f"{operation.path}: every line of an added file must start with '+'",
                        reason="parse",
                    )
                added.append(body[index][1:])
                index += 1
            operation.added = "".join(f"{text}\n" for text in added)
            operations.append(operation)
        elif line.startswith(_DELETE):
            operations.append(_Operation("delete", header_path(line, _DELETE)))
            index += 1
        elif line.startswith(_UPDATE):
            operation = _Operation("update", header_path(line, _UPDATE))
            index += 1
            if index < len(body) and body[index].startswith(_MOVE):
                operation.move_to = header_path(body[index], _MOVE)
                index += 1
            hunk: _Hunk | None = None
            while index < len(body):
                line = body[index]
                if line.strip() == _END_OF_FILE:
                    if hunk is None:
                        raise PatchError(
                            f"{operation.path}: {_END_OF_FILE!r} outside a hunk", reason="parse"
                        )
                    hunk.at_eof = True
                    hunk = None
                elif line.startswith("*** "):
                    break
                elif line.startswith("@@"):
                    anchor = line[2:].strip()
                    if hunk is None or hunk.lines:
                        hunk = _Hunk()
                        operation.hunks.append(hunk)
                    if anchor:
                        hunk.anchors.append(anchor)
                elif line[:1] in (" ", "-", "+") or line == "":
                    if hunk is None:
                        # The first hunk of a file may omit its `@@` line.
                        hunk = _Hunk()
                        operation.hunks.append(hunk)
                    # A bare empty line has kind "": an empty context line,
                    # unless it trails the hunk (blank separator lines).
                    hunk.lines.append((line[:1], line[1:]))
                else:
                    raise PatchError(
                        f"{operation.path}: unexpected line {line!r} "
                        "(hunk lines start with ' ', '-', or '+')",
                        reason="parse",
                    )
                index += 1
            for number, parsed in enumerate(operation.hunks, start=1):
                while parsed.lines and parsed.lines[-1][0] == "":
                    parsed.lines.pop()
                parsed.lines = [(kind or " ", text) for kind, text in parsed.lines]
                if not any(kind in "-+" for kind, _ in parsed.lines):
                    raise PatchError(
                        f"{operation.path}: hunk {number}: no '-' or '+' lines", reason="parse"
                    )
            if not operation.hunks and operation.move_to is None:
                raise PatchError(f"{operation.path}: update has no hunks", reason="parse")
            operations.append(operation)
        else:
            raise PatchError(
                f"line {index + 2}: expected a file header, got {line!r}", reason="parse"
            )
    if not operations:
        raise PatchError("patch contains no file operations", reason="parse")
    return operations


def _split_lines(content: str) -> list[str]:
    """Lines with their terminators; the last one may lack ``\\n``."""
    parts = content.split("\n")
    lines = [f"{part}\n" for part in parts[:-1]]
    if parts[-1]:
        lines.append(parts[-1])
    return lines


def _seek(texts: list[str], pattern: list[str], start: int, at_eof: bool) -> int | None:
    """First line index >= start where ``pattern`` matches: exactly, then
    ignoring trailing whitespace, then ignoring surrounding whitespace.
    An end-of-file hunk tries the last possible position first."""
    last = len(texts) - len(pattern)
    if last < start:
        return None
    positions = list(range(start, last + 1))
    if at_eof:
        positions = [last] + positions[:-1]
    for normalize in (lambda text: text, str.rstrip, str.strip):
        for position in positions:
            if all(
                normalize(texts[position + offset]) == normalize(expected)
                for offset, expected in enumerate(pattern)
            ):
                return position
    return None


def _resolve(content: str, cursor: int, hunk: _Hunk, label: str) -> tuple[str, str, int]:
    """One hunk -> ``(old_str, new_str, cursor after the hunk)``.

    The hunk's lines are located at line granularity (Codex semantics:
    anchors, then the first match after ``cursor``); ``old_str`` is then
    widened by whole surrounding lines until it occurs exactly once in
    ``content``, which is what ``edit.run`` requires. Context lines keep the
    file's own text, so whitespace-tolerant matches never rewrite them.
    """
    lines = _split_lines(content)
    texts = [line.rstrip("\n") for line in lines]
    for anchor in hunk.anchors:
        found = next(
            (index for index in range(cursor, len(texts)) if texts[index].strip() == anchor),
            None,
        )
        if found is None:
            raise PatchError(
                f"{label}: anchor {anchor!r} not found", reason="context_not_found"
            )
        cursor = found + 1
    pattern = [text for kind, text in hunk.lines if kind != "+"]
    if pattern:
        start = _seek(texts, pattern, cursor, hunk.at_eof)
        if start is None:
            preview = "\n".join(pattern[:3])
            raise PatchError(
                f"{label}: context not found:\n{preview}", reason="context_not_found"
            )
    else:
        # A pure insertion goes right after its anchor, or at end of file.
        start = cursor if hunk.anchors else len(lines)
    end = start + len(pattern)

    replacement: list[str] = []
    position = start
    for kind, text in hunk.lines:
        if kind == " ":
            replacement.append(lines[position])
            position += 1
        elif kind == "-":
            position += 1
        else:
            replacement.append(f"{text}\n")

    # Widen by whole lines until the span's text starts at exactly one offset
    # (overlapping occurrences included), so edit.run's single replacement
    # lands on this span and nowhere else.
    offsets = [0]
    for line in lines:
        offsets.append(offsets[-1] + len(line))
    low, high = start, end
    grow_up = True
    while True:
        old_str = "".join(lines[low:high])
        if content.find(old_str) == offsets[low] == content.rfind(old_str):
            break
        if (grow_up and low > 0) or high >= len(lines):
            low -= 1
        else:
            high += 1
        grow_up = not grow_up
    # Every new line ends with "\n", except that a file which ended without
    # a newline still does.
    region = lines[low:start] + replacement + lines[end:high]
    new_str = "".join(line if line.endswith("\n") else f"{line}\n" for line in region)
    if new_str and old_str and not old_str.endswith("\n"):
        new_str = new_str[:-1]
    return old_str, new_str, start + len(replacement)


class _Workspace:
    """The in-memory view of every file the patch touches."""

    def __init__(self) -> None:
        self._contents: dict[Path, str | None] = {}

    @staticmethod
    def key(path: str) -> Path:
        return Path(path).expanduser().resolve()

    def exists(self, path: str) -> bool:
        key = self.key(path)
        if key in self._contents:
            return self._contents[key] is not None
        return key.exists()

    def read(self, path: str) -> str:
        key = self.key(path)
        if key in self._contents:
            content = self._contents[key]
            if content is None:
                raise PatchError(f"{path}: deleted earlier in this patch", reason="file_missing")
            return content
        if not key.is_file():
            raise PatchError(f"{path}: file not found", reason="file_missing")
        try:
            # Read exactly as edit.run reads, so validation sees its content.
            content = key.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise PatchError(f"{path}: cannot read: {error}", reason="file_missing") from error
        self._contents[key] = content
        return content

    def write(self, path: str, content: str | None) -> None:
        self._contents[self.key(path)] = content


def _validate(operations: list[_Operation]) -> list[list[tuple[str, str]]]:
    """Phase 1: the ``(old_str, new_str)`` pairs per operation, or PatchError."""
    workspace = _Workspace()
    plan: list[list[tuple[str, str]]] = []
    for operation in operations:
        pairs: list[tuple[str, str]] = []
        if operation.kind == "add":
            if workspace.exists(operation.path):
                raise PatchError(f"{operation.path}: file already exists", reason="file_exists")
            workspace.write(operation.path, operation.added)
        elif operation.kind == "delete":
            workspace.read(operation.path)
            workspace.write(operation.path, None)
        else:
            content = workspace.read(operation.path)
            cursor = 0
            for number, hunk in enumerate(operation.hunks, start=1):
                # Resolved against the content as earlier hunks left it (the
                # replay edit.run will see), so an earlier hunk's new text can
                # never make a later hunk ambiguous at write time.
                old_str, new_str, cursor = _resolve(
                    content, cursor, hunk, f"{operation.path}: hunk {number}"
                )
                content = content.replace(old_str, new_str, 1)
                pairs.append((old_str, new_str))
            if operation.move_to is not None:
                moved = workspace.key(operation.move_to) != workspace.key(operation.path)
                if moved and workspace.exists(operation.move_to):
                    raise PatchError(
                        f"{operation.path}: move target {operation.move_to} already exists",
                        reason="file_exists",
                    )
                workspace.write(operation.path, None)
                workspace.write(operation.move_to, content)
            else:
                workspace.write(operation.path, content)
        plan.append(pairs)
    return plan


async def _report(files: int, hunks: int, failure: str | None) -> None:
    """Adoption counters to the host (best-effort; absent outside a kernel)."""
    try:
        from rlm import host_request

        await host_request(_REPORT_REQUEST, {"files": files, "hunks": hunks, "failure": failure})
    except Exception:
        pass


async def run(patch: str) -> list[dict[str, Any]]:
    """Apply one V4A patch (``*** Begin Patch`` ... ``*** End Patch``).

    Every file and hunk is validated in memory first; on any failure a
    :class:`PatchError` names the file, the hunk, and the reason, and no file
    is touched. Then each update hunk is written through ``edit.run`` (same
    writes and diff display as direct ``edit`` calls), added files are
    created, deleted files unlinked, and moved files renamed.

    Args:
        patch: The patch text. Sections: ``*** Update File: <path>``
            (optionally followed by ``*** Move to: <path>``), ``*** Add File:
            <path>`` (``+`` lines), ``*** Delete File: <path>``. Update hunks
            start with ``@@`` or ``@@ <anchor line>``, then `` ``/``-``/``+``
            lines; ``*** End of File`` anchors a hunk at the end of the file.

    Returns:
        One summary per file operation, in patch order:
        ``{"path", "operation", "hunks", "move_to"}``.

    Raises:
        PatchError: The patch is malformed, a file is missing or already
            exists, or a hunk's context is not found.
    """
    try:
        operations = _parse(patch)
        plan = _validate(operations)
    except PatchError as error:
        await _report(0, 0, error.reason)
        raise
    summary: list[dict[str, Any]] = []
    for operation, pairs in zip(operations, plan):
        try:
            if operation.kind == "add":
                target = Path(operation.path).expanduser()
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(operation.added, encoding="utf-8")
                edit._emit_diff(str(target.resolve()), "", operation.added, 1)
            elif operation.kind == "delete":
                Path(operation.path).expanduser().unlink()
            else:
                for old_str, new_str in pairs:
                    await edit.run(operation.path, old_str, new_str)
                if operation.move_to is not None:
                    target = Path(operation.move_to).expanduser()
                    target.parent.mkdir(parents=True, exist_ok=True)
                    Path(operation.path).expanduser().replace(target)
        except (OSError, ValueError) as error:
            await _report(len(summary), sum(entry["hunks"] for entry in summary), "apply_failed")
            raise PatchError(
                f"{operation.path}: applying failed after validation (the file changed "
                f"on disk?): {error}; earlier files of this patch were already written",
                reason="apply_failed",
            ) from error
        summary.append(
            {
                "path": operation.path,
                "operation": operation.kind,
                "hunks": len(pairs),
                "move_to": operation.move_to,
            }
        )
    await _report(len(summary), sum(entry["hunks"] for entry in summary), None)
    return summary
