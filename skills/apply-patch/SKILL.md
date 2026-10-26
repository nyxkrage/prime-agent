---
name: apply-patch
description: Edit, add, delete, or move files with one V4A patch (*** Begin Patch ... *** End Patch), the OpenAI Codex apply_patch format. The whole patch is checked before anything is written, and a failure while writing undoes every change already made.
---

# Apply Patch

Apply one V4A patch (OpenAI Codex's `apply_patch` format) from the kernel:

    await apply_patch(r'''*** Begin Patch
    *** Update File: src/app.py
    @@ def main():
    -    print("hi")
    +    print("hello")
    *** End Patch''')

Sections:

- `*** Add File: <path>`: every following line starts with `+`. Missing parent
  directories are created; an existing file is replaced.
- `*** Delete File: <path>`: nothing follows. A symlink is removed, not its target.
- `*** Update File: <path>`: optionally `*** Move to: <new path>` (replacing an
  existing file there), then one or more hunks.

A hunk starts with `@@`, or `@@ <line>` to search after the first line matching
`<line>`. Its lines start with ` ` (context), `-` (removed), or `+` (added).
`*** End of File` after a hunk pins it to the end of the file. Hunks apply in
order, each searched after the previous one. Lines match exactly, then ignoring
trailing whitespace, then surrounding whitespace, then typographic quotes and
dashes. A hunk with no context or `-` lines appends to the end of the file.
Updated files keep their line endings and end with a newline.

The whole patch is checked first: a malformed patch, a missing or non-UTF-8
file, lines not found, a blocked path, or a missing write permission raises
`apply_patch.PatchError` (naming the file, the hunk, and the reason) and no file
is changed. Files are then written in place (keeping permissions, ownership,
and hardlinks, like `edit`); if a write fails or the run is interrupted, every
change already made is undone and `PatchError` (reason `apply_failed`) says so.
Only a crash of the kernel process mid-write, or a failing undo (listed in the
error), can leave a patch partly applied.

Each changed hunk shows its diff like a direct `edit` call. Returns one summary
per file: `{"path", "operation", "hunks", "move_to"}`.

Write the patch as a raw string, `r'''...'''`: in a plain `'''...'''` literal
Python turns `\n`, `\t`, or `\d` in the patched code into other characters. If
the patch contains `'''`, use `r"""..."""`.
