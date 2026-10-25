---
name: apply-patch
description: Edit, add, delete, or move files with one V4A patch (*** Begin Patch ... *** End Patch), the OpenAI Codex apply_patch format. The whole patch is checked before anything is written, and a failure while writing undoes every change already made.
---

# Apply Patch

Apply one patch in the V4A format (the format of OpenAI Codex's `apply_patch`)
from the kernel:

    await apply_patch(r'''*** Begin Patch
    *** Update File: src/app.py
    @@ def main():
    -    print("hi")
    +    print("hello")
    *** End Patch''')

Sections:

- `*** Add File: <path>`: every following line starts with `+`. Missing parent
  directories are created, and an existing file is replaced.
- `*** Delete File: <path>`: nothing follows. A symlink is removed itself, not
  its target.
- `*** Update File: <path>`: optionally followed by `*** Move to: <new path>`
  (which replaces an existing file there), then one or more hunks.

A hunk starts with `@@`, or with `@@ <line>` to search after the first line
matching `<line>` (one `@@` line per hunk). Its lines start with ` ` (context),
`-` (removed), or `+` (added). `*** End of File` after a hunk pins it to the end
of the file. Hunks of one file apply in order, each searched after the previous
one. Lines match exactly first, then ignoring trailing whitespace, then
surrounding whitespace, then typographic quotes and dashes. A hunk with no
context or `-` lines appends to the end of the file (before a final empty
line, as in Codex). Updated files keep their line endings and always end with
a newline.

The whole patch is checked first. If anything would fail (a malformed patch, a
missing or non-UTF-8 file to update, lines not found, a path blocked by a file
or directory, no write permission), `apply_patch.PatchError` is raised naming
the file, the hunk where relevant, and the reason, and no file is changed.

Then every file to rewrite is opened for writing, and only after that does
anything change: files are removed, directories and new files created, and
existing files rewritten in place (so they keep their permissions, ownership,
hardlinks, and other metadata, exactly like `edit`). If a write fails or the
run is interrupted, every change already made is undone and `PatchError`
(reason `apply_failed`) says so; a file the patch removed or moved away is
recreated with its content and permissions. Only a crash of the kernel process
itself while writing, or an undo that fails too (the error lists those files),
can leave a patch partly applied. No helper files are ever written next to your files.

Each changed hunk shows its diff exactly like a direct `edit` call. Returns one
summary per file: `{"path", "operation", "hunks", "move_to"}`.

Write the patch as a raw string, `r'''...'''`: in a plain `'''...'''` literal
Python interprets backslashes, so `\n`, `\t`, or `\d` in the code you patch
would turn into other characters before `apply_patch` sees them. If the patch
text contains `'''`, use `r"""..."""` instead.
