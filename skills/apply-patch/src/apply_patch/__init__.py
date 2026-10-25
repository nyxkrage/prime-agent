"""Apply V4A patches (the OpenAI Codex ``apply_patch`` format) from the kernel.

Parsing and matching follow Codex's ``codex-rs/apply-patch`` (the lenient
streaming parser, ``seek_sequence``'s whitespace/punctuation tolerance, the
line-based replacement with preserved line endings, and the Add/Delete/Move
semantics). Files are written in place, as Codex and ``edit.run`` write them,
so a rewritten file keeps its identity (hardlinks, permissions, ownership,
extended attributes). The one deliberate difference is atomicity: Codex
applies file by file and can stop halfway, while here the whole patch is
first replayed against an in-memory view of the filesystem (contents,
existence, directory structure, write access), then every file to rewrite is
opened for writing before anything changes, and a failure or interruption
while writing undoes every change already made. Each changed hunk emits the
same diff display event as the equivalent ``edit.run`` call.
"""

from __future__ import annotations

import os
import re
import stat
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, BinaryIO

import edit

_BEGIN = "*** Begin Patch"
_END = "*** End Patch"
_ADD = "*** Add File: "
_DELETE = "*** Delete File: "
_UPDATE = "*** Update File: "
_MOVE = "*** Move to: "
_END_OF_FILE = "*** End of File"
_CONTEXT = "@@ "
_EMPTY_CONTEXT = "@@"
_HEREDOC_OPENERS = ("<<EOF", "<<'EOF'", '<<"EOF"')

# Host request carrying the adoption counters (no paths, no content).
_REPORT_REQUEST = "apply_patch.report"

_LINE_ENDING = re.compile(r"\r\n|\r|\n")

# seek_sequence's last pass: typographic punctuation and odd spaces read as ASCII.
_PUNCTUATION = str.maketrans(
    {
        **dict.fromkeys("‐‑‒–—―−", "-"),
        **dict.fromkeys("‘’‚‛", "'"),
        **dict.fromkeys("“”„‟", '"'),
        **dict.fromkeys(
            "            　",
            " ",
        ),
    }
)


class PatchError(ValueError):
    """A patch that failed to parse, validate, or write.

    ``reason`` is one of ``parse``, ``context_not_found``, ``file_missing``,
    ``path_conflict``, ``not_writable``, ``unreadable``, ``apply_failed``
    (keep in sync with ``APPLY_PATCH_FAILURE_REASONS`` in
    ``crates/pa-telemetry/src/catalog.rs``). Every reason except
    ``apply_failed`` is raised before anything is written. ``apply_failed``
    means a filesystem operation failed while the patch was being written;
    every change already made was undone, unless the message lists files
    whose undo failed too.
    """

    def __init__(self, message: str, *, reason: str) -> None:
        super().__init__(message)
        self.reason = reason


def _invalid_patch(message: str) -> PatchError:
    return PatchError(f"Invalid patch: {message}", reason="parse")


def _invalid_hunk(message: str, line_number: int) -> PatchError:
    return PatchError(f"Invalid patch hunk on line {line_number}: {message}", reason="parse")


@dataclass
class _Chunk:
    """One ``@@`` section: ``old`` lines replaced by ``new`` lines."""

    context: str | None = None
    old: list[str] = field(default_factory=list)
    new: list[str] = field(default_factory=list)
    # (old index, new index) of lines parsed as context, kept from the file.
    context_indices: list[tuple[int, int]] = field(default_factory=list)
    at_eof: bool = False

    def push_context(self, line: str) -> None:
        self.context_indices.append((len(self.old), len(self.new)))
        self.old.append(line)
        self.new.append(line)


@dataclass
class _Operation:
    kind: str  # "add" | "delete" | "update"
    path: str
    line_number: int
    move_to: str | None = None
    chunks: list[_Chunk] = field(default_factory=list)
    contents: str = ""


class _Parser:
    """Codex's ``StreamingPatchParser`` fed one whole patch."""

    def __init__(self) -> None:
        self.mode = "not_started"  # | started | add | delete | update | ended
        self.operations: list[_Operation] = []
        self.line_number = 0

    def _unexpected(self, line: str) -> PatchError:
        return _invalid_hunk(
            f"Unexpected line found in update hunk: '{line}'. Every line should start with "
            "' ' (context line), '+' (added line), or '-' (removed line)",
            self.line_number,
        )

    def _ensure_update_is_not_empty(self, line: str) -> None:
        if not self.operations or self.operations[-1].kind != "update":
            return
        operation = self.operations[-1]
        if not operation.chunks and self.mode == "update":
            raise _invalid_hunk(
                f"Update file hunk for path '{operation.path}' is empty", operation.line_number
            )
        if operation.chunks and not operation.chunks[-1].old and not operation.chunks[-1].new:
            if line == _END:
                raise _invalid_hunk("Update hunk does not contain any lines", self.line_number)
            raise self._unexpected(line)

    def _header_or_end(self, line: str) -> bool:
        if line == _END:
            self._ensure_update_is_not_empty(line)
            self.mode = "ended"
            return True
        for marker, kind in ((_ADD, "add"), (_DELETE, "delete"), (_UPDATE, "update")):
            if line.startswith(marker):
                self._ensure_update_is_not_empty(line)
                self.operations.append(_Operation(kind, line[len(marker) :], self.line_number))
                self.mode = kind
                return True
        return False

    def process(self, line: str) -> None:
        self.line_number += 1
        trimmed = line.strip()
        if self.mode == "not_started":
            if trimmed != _BEGIN:
                raise _invalid_patch("The first line of the patch must be '*** Begin Patch'")
            self.mode = "started"
            return
        if self.mode == "ended":
            if trimmed:
                raise _invalid_patch("The last line of the patch must be '*** End Patch'")
            return
        if self.mode != "update":
            if self._header_or_end(trimmed):
                return
            if self.mode != "add" or not line.startswith("+"):
                raise _invalid_hunk(
                    f"'{trimmed}' is not a valid hunk header. Valid hunk headers: "
                    "'*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'",
                    self.line_number,
                )
            self.operations[-1].contents += f"{line[1:]}\n"
            return
        # Inside an update only unindented headers count; an indented one is
        # a context line (Codex).
        update_line = line.rstrip()
        if self._header_or_end(update_line):
            return
        operation = self.operations[-1]
        chunks = operation.chunks
        is_context = update_line == _EMPTY_CONTEXT or update_line.startswith(_CONTEXT)
        if chunks and chunks[-1].at_eof:
            if not update_line:
                return
            if not is_context:
                raise _invalid_hunk(
                    f"Expected update hunk to start with a @@ context marker, got: '{line}'",
                    self.line_number,
                )
        if not chunks and operation.move_to is None and update_line.startswith(_MOVE):
            operation.move_to = update_line[len(_MOVE) :]
            return
        last_is_empty = bool(chunks) and not chunks[-1].old and not chunks[-1].new
        if is_context:
            # Consecutive `@@` lines (nested anchors) are rejected, as in Codex.
            if last_is_empty:
                raise self._unexpected(line)
            context = None if update_line == _EMPTY_CONTEXT else update_line[len(_CONTEXT) :]
            chunks.append(_Chunk(context=context))
            return
        if update_line == _END_OF_FILE:
            if last_is_empty:
                raise _invalid_hunk("Update hunk does not contain any lines", self.line_number)
            if chunks:
                chunks[-1].at_eof = True
            return
        if line == "" or line[0] in " +-":
            if not chunks:
                # The first chunk of a file may omit its `@@` line.
                chunks.append(_Chunk())
            if line == "":
                chunks[-1].push_context("")
            elif line[0] == " ":
                chunks[-1].push_context(line[1:])
            elif line[0] == "+":
                chunks[-1].new.append(line[1:])
            else:
                chunks[-1].old.append(line[1:])
            return
        if chunks and (chunks[-1].old or chunks[-1].new):
            raise _invalid_hunk(
                f"Expected update hunk to start with a @@ context marker, got: '{line}'",
                self.line_number,
            )
        raise self._unexpected(line)


def _parse(patch: str) -> list[_Operation]:
    """Codex's ``parse_patch`` (lenient mode): envelope, heredoc tolerance, hunks."""
    stripped = patch.strip()
    lines = [line.removesuffix("\r") for line in stripped.split("\n")] if stripped else []

    def check_envelope(candidate: list[str]) -> list[str]:
        first = candidate[0].strip() if candidate else None
        last = candidate[-1].strip() if candidate else None
        if first == _BEGIN and last == _END:
            return candidate
        if first is not None and first != _BEGIN:
            raise _invalid_patch("The first line of the patch must be '*** Begin Patch'")
        raise _invalid_patch("The last line of the patch must be '*** End Patch'")

    try:
        lines = check_envelope(lines)
    except PatchError:
        # A heredoc-wrapped patch (`<<'EOF'` ... `EOF`) is unwrapped.
        if not (len(lines) >= 4 and lines[0] in _HEREDOC_OPENERS and lines[-1].endswith("EOF")):
            raise
        lines = check_envelope(lines[1:-1])
    parser = _Parser()
    for line in lines[:-1]:
        parser.process(line.removesuffix("\r"))
    parser.line_number += 1
    parser._ensure_update_is_not_empty(_END)
    return parser.operations


def _seek(lines: list[str], pattern: list[str], start: int, at_eof: bool) -> int | None:
    """Codex's ``seek_sequence``: the first index >= ``start`` where ``pattern``
    matches, exactly, then ignoring trailing whitespace, then surrounding
    whitespace, then typographic punctuation. An end-of-file pattern is only
    tried at the last possible position."""
    if not pattern:
        return start
    last = len(lines) - len(pattern)
    if last < 0:
        return None
    first = max(last, start) if at_eof else start
    for normalize in (
        None,
        str.rstrip,
        str.strip,
        lambda text: text.strip().translate(_PUNCTUATION),
    ):
        for index in range(first, last + 1):
            window = lines[index : index + len(pattern)]
            if normalize is None:
                if window == pattern:
                    return index
            elif all(normalize(got) == normalize(want) for got, want in zip(window, pattern)):
                return index
    return None


# (start line, old line count, new lines)
_Replacement = tuple[int, int, list[str]]


def _replacements(
    texts: list[str], chunks: list[_Chunk], path: str
) -> list[tuple[int, int, list[_Replacement]]]:
    """Codex's ``compute_replacements``, grouped per chunk as
    ``(span start, span length, replacements)`` in file order."""
    spans: list[tuple[int, int, list[_Replacement]]] = []
    line_index = 0
    for number, chunk in enumerate(chunks, start=1):
        if chunk.context is not None:
            found = _seek(texts, [chunk.context], line_index, False)
            if found is None:
                raise PatchError(
                    f"Failed to find context '{chunk.context}' in {path} (hunk {number})",
                    reason="context_not_found",
                )
            line_index = found + 1
        if not chunk.old:
            # A pure addition goes at the end of the file, before its last
            # line when that line is empty (Codex).
            at = len(texts) - 1 if texts and texts[-1] == "" else len(texts)
            spans.append((at, 0, [(at, 0, list(chunk.new))]))
            continue
        pattern, new = chunk.old, chunk.new
        start = _seek(texts, pattern, line_index, chunk.at_eof)
        if start is None and pattern[-1] == "":
            # A trailing empty line stands for the file's final newline.
            pattern = pattern[:-1]
            if new and new[-1] == "":
                new = new[:-1]
            start = _seek(texts, pattern, line_index, chunk.at_eof)
        if start is None:
            joined = "\n".join(chunk.old)
            raise PatchError(
                f"Failed to find expected lines in {path} (hunk {number}):\n{joined}",
                reason="context_not_found",
            )
        # Context lines stay as the file has them (text and line ending).
        replacements: list[_Replacement] = []
        old_start = new_start = 0
        for old_context, new_context in chunk.context_indices:
            if old_context >= len(pattern) or new_context >= len(new):
                break
            if (old_start, new_start) != (old_context, new_context):
                replacements.append(
                    (start + old_start, old_context - old_start, new[new_start:new_context])
                )
            old_start, new_start = old_context + 1, new_context + 1
        if (old_start, new_start) != (len(pattern), len(new)):
            replacements.append((start + old_start, len(pattern) - old_start, new[new_start:]))
        spans.append((start, len(pattern), replacements))
        line_index = start + len(pattern)
    spans.sort(key=lambda span: span[0])
    return spans


def _rebuild(
    lines: list[tuple[str, str | None]],
    offset: int,
    replacements: list[_Replacement],
    ending: str,
) -> list[tuple[str, str]]:
    """``lines`` (starting at file line ``offset``) with ``replacements``
    applied: kept lines keep their endings, new lines get ``ending``, and
    every line ends with a newline (Codex)."""
    rebuilt: list[tuple[str, str]] = []
    cursor = 0
    for start, old_len, new_lines in replacements:
        rebuilt.extend(
            (text, line_end or ending) for text, line_end in lines[cursor : start - offset]
        )
        rebuilt.extend((text, ending) for text in new_lines)
        cursor = start - offset + old_len
    rebuilt.extend((text, line_end or ending) for text, line_end in lines[cursor:])
    return rebuilt


def _joined(lines: list[tuple[str, str]]) -> str:
    return "".join(f"{text}{line_end}" for text, line_end in lines)


def _update(
    content: str, chunks: list[_Chunk], path: str
) -> tuple[str, list[tuple[str, str, int]]]:
    """New contents plus one ``(old_str, new_str, start_line)`` display event
    per changed chunk, as the equivalent sequence of ``edit.run`` calls
    would report them."""
    lines: list[tuple[str, str | None]] = []
    position = 0
    for match in _LINE_ENDING.finditer(content):
        lines.append((content[position : match.start()], match.group()))
        position = match.end()
    if position < len(content):
        lines.append((content[position:], None))
    ending = next((line_end for _, line_end in lines if line_end), "\n")
    spans = _replacements([text for text, _ in lines], chunks, path)
    events: list[tuple[str, str, int]] = []
    shift = 0
    for start, length, replacements in spans:
        span = lines[start : start + length]
        old_str = "".join(f"{text}{line_end or ''}" for text, line_end in span)
        rebuilt = _rebuild(span, start, replacements, ending)
        new_str = _joined(rebuilt)
        if old_str != new_str:
            events.append((old_str, new_str, start + 1 + shift))
        shift += len(rebuilt) - length
    new_lines = _rebuild(
        lines, 0, [replacement for _, _, group in spans for replacement in group], ending
    )
    # A final empty line reads as the file's last newline and is dropped
    # (Codex joins its lines with newlines and ends the file with one).
    if new_lines and new_lines[-1][0] == "":
        new_lines.pop()
    return _joined(new_lines), events


class _Workspace:
    """The patch's in-memory view of the filesystem.

    ``files`` maps a path to its planned content (``None``: removed);
    ``dirs`` holds the directories the patch creates; ``deleted`` holds the
    paths on disk the patch removes (a later write there creates a new
    file rather than rewriting the old one). Paths are keyed by their
    absolute spelling with the parent directory resolved, so a leaf symlink
    stays its own entry: Delete and Move remove the link itself, while reads
    and writes follow it to the file it names (Codex's default
    ``follow_symlinks``).
    """

    def __init__(self) -> None:
        self.files: dict[Path, str | None] = {}
        self.data: dict[Path, bytes] = {}
        self.dirs: set[Path] = set()
        self.deleted: set[Path] = set()
        self._texts: dict[Path, str] = {}

    @staticmethod
    def lexical(path: str) -> Path:
        absolute = Path(os.path.abspath(Path(path).expanduser()))
        return absolute.parent.resolve() / absolute.name

    def target(self, lexical: Path) -> Path:
        if lexical not in self.files and lexical.is_symlink():
            return Path(os.path.realpath(lexical))
        return lexical

    def _is_dir(self, path: Path) -> bool:
        return path not in self.files and (path in self.dirs or path.is_dir())

    def _check_dir_writable(self, directory: Path, path: str) -> None:
        if directory not in self.dirs and not os.access(directory, os.W_OK | os.X_OK):
            raise PatchError(
                f"Failed to write file {path}: directory {directory} is not writable",
                reason="not_writable",
            )

    def read(self, path: str) -> str:
        """The text an update of ``path`` starts from."""
        target = self.target(self.lexical(path))
        if target in self.files:
            content = self.files[target]
            if content is None:
                raise PatchError(
                    f"Failed to read file to update {path}: No such file or directory",
                    reason="file_missing",
                )
            return content
        if target in self._texts:
            return self._texts[target]
        if self._is_dir(target):
            raise PatchError(
                f"Failed to read file to update {path}: Is a directory", reason="path_conflict"
            )
        try:
            text = target.read_bytes().decode("utf-8")
        except FileNotFoundError:
            raise PatchError(
                f"Failed to read file to update {path}: No such file or directory",
                reason="file_missing",
            ) from None
        except (OSError, UnicodeDecodeError) as error:
            raise PatchError(
                f"Failed to read file to update {path}: {error}", reason="unreadable"
            ) from error
        self._texts[target] = text
        return text

    def write(self, path: str, content: str) -> tuple[Path, str]:
        """Plan ``content`` at ``path`` (creating missing parent directories,
        rewriting an existing file in place); returns the written file and
        the text it replaces (``""`` for a new or non-text file)."""
        target = self.target(self.lexical(path))
        if self._is_dir(target):
            raise PatchError(f"Failed to write file {path}: Is a directory", reason="path_conflict")
        try:
            data = content.encode("utf-8")
        except UnicodeEncodeError as error:
            # A lone surrogate, e.g. from an escape in a non-raw literal.
            raise PatchError(
                f"Invalid patch: the new contents of {path} are not valid UTF-8 text: {error}",
                reason="parse",
            ) from None
        # An existing file is rewritten in place, which needs write access to
        # the file only; a new one needs its directory writable.
        rewrite = os.path.lexists(target) and target not in self.deleted
        missing: list[Path] = []
        for parent in target.parents:
            if parent in self.files:
                if self.files[parent] is not None:
                    raise PatchError(
                        f"Failed to create parent directories for {path}: {parent} is a file",
                        reason="path_conflict",
                    )
                missing.append(parent)
            elif self._is_dir(parent):
                if not rewrite:
                    self._check_dir_writable(parent, path)
                break
            elif os.path.lexists(parent):
                raise PatchError(
                    f"Failed to create parent directories for {path}: {parent} is not a directory",
                    reason="path_conflict",
                )
            else:
                missing.append(parent)
        if rewrite and not os.access(target, os.W_OK):
            raise PatchError(
                f"Failed to write file {path}: Permission denied", reason="not_writable"
            )
        try:
            previous = self.read(path) if target in self.files or rewrite else ""
        except PatchError:
            previous = ""
        self.dirs.update(missing)
        self.files[target] = content
        self.data[target] = data
        return target, previous

    def delete(self, path: str) -> None:
        """Plan the removal of ``path`` (a symlink itself, not its target)."""
        lexical = self.lexical(path)
        if lexical in self.files:
            if self.files[lexical] is None:
                raise PatchError(
                    f"Failed to delete file {path}: No such file or directory",
                    reason="file_missing",
                )
        else:
            target = self.target(lexical)
            if self._is_dir(lexical) or self._is_dir(target):
                raise PatchError(
                    f"Failed to delete file {path}: path is a directory", reason="path_conflict"
                )
            exists = (
                self.files[target] is not None if target in self.files else os.path.exists(target)
            )
            if not exists:
                raise PatchError(
                    f"Failed to delete file {path}: No such file or directory",
                    reason="file_missing",
                )
        if os.path.lexists(lexical):
            self._check_dir_writable(lexical.parent, path)
            self.deleted.add(lexical)
        self.files[lexical] = None


def _validate(
    operations: list[_Operation],
) -> tuple[_Workspace, list[tuple[str, str, str, int]]]:
    """Phase 1: replay every operation in memory, in patch order, as Codex
    would apply it; returns the final view and the diff display events."""
    if not operations:
        raise PatchError("No files were modified.", reason="parse")
    workspace = _Workspace()
    events: list[tuple[str, str, str, int]] = []
    for operation in operations:
        if operation.kind == "add":
            target, previous = workspace.write(operation.path, operation.contents)
            events.append((str(target), previous, operation.contents, 1))
        elif operation.kind == "delete":
            workspace.delete(operation.path)
        else:
            content = workspace.read(operation.path)
            new_content, hunk_events = _update(content, operation.chunks, operation.path)
            destination = operation.move_to
            if destination is None or workspace.lexical(destination) == workspace.lexical(
                operation.path
            ):
                target, _ = workspace.write(operation.path, new_content)
            else:
                target, _ = workspace.write(destination, new_content)
                workspace.delete(operation.path)
            events.extend((str(target), old, new, line) for old, new, line in hunk_events)
    return workspace, events


class _Journal:
    """Every filesystem change the commit makes, in order, with what undoes
    it: rewritten files keep their open handle and original bytes, removed
    files their bytes (or link target) and permission bits. Nothing is
    written outside the patch's own paths, so no run leaves helper files
    behind."""

    def __init__(self) -> None:
        self.handles: dict[Path, tuple[BinaryIO, bytes]] = {}
        self.backups: dict[Path, tuple[bytes | str, os.stat_result]] = {}
        # ("rewrite" | "create" | "mkdir" | "unlink", path), in the order done.
        self.done: list[tuple[str, Path]] = []

    def open_for_rewrite(self, path: Path) -> None:
        # Opening for writing proves the rewrite can happen: no permission,
        # a read-only mount, or (on Windows) a lock fails here, before any
        # content changes.
        handle = open(path, "r+b")
        self.handles[path] = (handle, handle.read())

    def back_up(self, path: Path) -> None:
        status = os.lstat(path)
        if stat.S_ISLNK(status.st_mode):
            self.backups[path] = (os.readlink(path), status)
        else:
            with open(path, "rb") as handle:
                self.backups[path] = (handle.read(), status)

    def unlink(self, path: Path) -> None:
        os.unlink(path)
        self.done.append(("unlink", path))

    def mkdir(self, path: Path) -> None:
        os.mkdir(path)
        self.done.append(("mkdir", path))

    def create(self, path: Path, data: bytes) -> None:
        # "x": a file that appeared since validation is never clobbered (the
        # patch fails and rolls back instead).
        with open(path, "xb") as handle:
            self.done.append(("create", path))
            handle.write(data)

    def rewrite(self, path: Path, data: bytes) -> None:
        handle, _ = self.handles[path]
        self.done.append(("rewrite", path))
        _write_over(handle, data)

    def roll_back(self) -> list[str]:
        """Undo every change, newest first; returns what could not be undone."""
        failures: list[str] = []
        for action, path in reversed(self.done):
            try:
                if action == "rewrite":
                    handle, original = self.handles[path]
                    _write_over(handle, original)
                elif action == "create":
                    os.unlink(path)
                elif action == "mkdir":
                    os.rmdir(path)
                else:
                    self._recreate(path)
            except OSError as error:
                failures.append(f"{path} ({error})")
        self.done.clear()
        return failures

    def _recreate(self, path: Path) -> None:
        backup, status = self.backups[path]
        if isinstance(backup, str):
            os.symlink(backup, path)
            return
        with open(path, "xb") as handle:
            handle.write(backup)
        os.chmod(path, stat.S_IMODE(status.st_mode))
        # Ownership is best-effort and absent on Windows.
        chown = getattr(os, "chown", None)
        if chown is not None:
            try:
                chown(path, status.st_uid, status.st_gid)
            except OSError:
                pass

    def close(self) -> None:
        for handle, _ in self.handles.values():
            try:
                handle.close()
            except OSError:
                pass


def _write_over(handle: BinaryIO, data: bytes) -> None:
    handle.seek(0)
    handle.write(data)
    handle.truncate()
    handle.flush()


def _commit(workspace: _Workspace) -> None:
    """Phase 2: open every file to rewrite (proving it writable) and back up
    every file to remove, changing nothing; then remove, create directories,
    create new files, and rewrite existing files in place. If any step fails
    or the run is interrupted, every change made so far is undone."""
    unlinks: list[Path] = []
    creates: list[Path] = []
    rewrites: list[Path] = []
    for path, content in workspace.files.items():
        on_disk = os.path.lexists(path)
        if content is None:
            if on_disk:
                unlinks.append(path)
        elif on_disk and path in workspace.deleted:
            unlinks.append(path)
            creates.append(path)
        elif on_disk:
            rewrites.append(path)
        else:
            creates.append(path)
    journal = _Journal()
    current = ""
    try:
        for path in rewrites:
            current = str(path)
            journal.open_for_rewrite(path)
        for path in unlinks:
            current = str(path)
            journal.back_up(path)
        for path in unlinks:
            current = str(path)
            journal.unlink(path)
        for directory in sorted(workspace.dirs, key=lambda directory: len(directory.parts)):
            if not directory.is_dir():
                current = str(directory)
                journal.mkdir(directory)
        for path in creates:
            current = str(path)
            journal.create(path, workspace.data[path])
        for path in rewrites:
            current = str(path)
            journal.rewrite(path, workspace.data[path])
    except BaseException as error:
        failures = journal.roll_back()
        journal.close()
        outcome = (
            f"could not undo the changes to: {', '.join(failures)}"
            if failures
            else "every change was undone"
        )
        if isinstance(error, OSError):
            raise PatchError(
                f"Failed to write {current}: {error}; {outcome}", reason="apply_failed"
            ) from error
        if failures and hasattr(error, "add_note"):
            error.add_note(f"apply_patch: {outcome}")
        raise
    journal.close()


async def _report(files: int, hunks: int, *, failure: str | None) -> None:
    """Adoption counters to the host (best-effort; absent outside a kernel)."""
    try:
        from rlm import host_request

        await host_request(_REPORT_REQUEST, {"files": files, "hunks": hunks, "failure": failure})
    except Exception:
        pass


async def run(patch: str) -> list[dict[str, Any]]:
    """Apply one V4A patch (``*** Begin Patch`` ... ``*** End Patch``).

    The whole patch is parsed and replayed in memory first; any failure
    raises :class:`PatchError` and no file is touched. Then every change is
    written (existing files in place); a failure or interruption while
    writing undoes every change already made. Each changed hunk emits the
    same diff display event as the equivalent ``edit.run`` call.

    Args:
        patch: The patch text. Sections: ``*** Add File: <path>`` (``+``
            lines; replaces an existing file), ``*** Delete File: <path>``,
            ``*** Update File: <path>`` (optionally followed by ``*** Move to:
            <path>``, which replaces an existing destination). Update hunks
            start with ``@@`` or ``@@ <line>`` (the hunk is searched after the
            first line matching it), then `` ``/``-``/``+`` lines;
            ``*** End of File`` pins a hunk to the end of the file. Hunks
            apply in order, each searched after the previous one; a hunk with
            no context or ``-`` lines appends to the end of the file.

    Returns:
        One summary per file operation, in patch order:
        ``{"path", "operation", "hunks", "move_to"}``.

    Raises:
        PatchError: The patch is malformed, a file is missing, unreadable,
            or not writable, a path is blocked, a hunk's lines are not found,
            or a write failed (see :class:`PatchError`).
    """
    try:
        operations = _parse(patch)
        workspace, events = _validate(operations)
        _commit(workspace)
    except PatchError as error:
        await _report(0, 0, failure=error.reason)
        raise
    for path, old_str, new_str, start_line in events:
        edit._emit_diff(path, old_str, new_str, start_line)
    summary = [
        {
            "path": operation.path,
            "operation": operation.kind,
            "hunks": len(operation.chunks),
            "move_to": operation.move_to,
        }
        for operation in operations
    ]
    await _report(len(summary), sum(entry["hunks"] for entry in summary), failure=None)
    return summary
