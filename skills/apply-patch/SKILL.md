---
name: apply-patch
description: Edit, add, delete, or move files with one OpenAI V4A patch (*** Begin Patch ... *** End Patch). The whole patch is validated before anything is written, then every hunk is applied through edit.
---

# Apply Patch

Apply one patch in the V4A format from the kernel:

    await apply_patch('''*** Begin Patch
    *** Update File: src/app.py
    @@ def main():
    -    print("hi")
    +    print("hello")
    *** End Patch''')

Sections: `*** Update File: <path>` (optionally followed by
`*** Move to: <new path>`), `*** Add File: <path>` (every line `+`), and
`*** Delete File: <path>`. Update hunks start with `@@` (optionally
`@@ <line that precedes the hunk>` to anchor it), then lines prefixed with
` ` (context), `-` (removed), or `+` (added); `*** End of File` anchors a
hunk at the end of the file. Hunks of one file apply in order, each searched
after the previous one.

The whole patch is validated first: if any file or hunk fails (missing file,
context not found, a malformed patch), `apply_patch.PatchError` is raised
naming the file, the hunk, and the reason, and no file is changed. Then each
update hunk is written through `edit.run`, so diffs display exactly like
direct `edit` calls. Returns one summary per file:
`{"path", "operation", "hunks", "move_to"}`.

If the patch text contains `'''`, wrap it in `"""` instead.
