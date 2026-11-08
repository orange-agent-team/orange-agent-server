# Single-file patch preparation — isolated implementation

This is the first TK-05/AG-007.02 component, not a registered executable tool. It performs no filesystem access, creates no approval and changes no files. The existing TK-06 creation permission cannot authorize editing.

`PatchArgs` contains a workspace-relative path, exact original SHA256 and a UTF-8 unified-hunk body. Unknown fields, protected paths, file headers, add/delete/rename directives and multiple target files are refused. The hunk body must end in LF. Context and removed lines match exactly; there is no fuzzy matching, offset guessing or line-ending normalization. CRLF and explicit no-final-newline markers are preserved. Empty replacement content does not delete a file.

Bounds are1 MiB for original/result text,64 KiB for the patch and128 hunks. NUL/binary/non-UTF8 source is rejected. Header ranges, overlap, output positions and old/new line counts are checked. The prepared result holds exact output bytes and a preview with input digest, old/new hashes, byte counts, line counts and complete displayed diff. This preview is a pure transformation of caller-provided bytes, not proof that a live file is authorized or unchanged.

Eight tests and strict all-target Runtime Clippy passed. Normal Agent dispatch, persistent edit consent/approval, publication, source lineage and Desktop integration remain unimplemented. This module must not be counted as complete apply_patch support.

## Execution work still required

The executor must read the real file under paired read/edit authority, bind the exact preview to the committed model response and tool contract, revalidate file identity and content before effects, and preserve both user and proposed versions on conflicts. A simple hash check followed by an unconditional overwriting rename is insufficient: another writer can change the target between those operations.

Publication must use reviewed target-platform primitives, keep durable recovery evidence, and never delete or overwrite an unrecognized competing version. Uncertain effects cannot be replayed. Do not advertise a global atomic content-CAS guarantee for uncooperative external filesystem writers without proof. Any capture/backup publication design must distinguish a clean conflict from a partially applied/unknown outcome and provide bounded recovery/retention behavior.

Successful edits also need an explicit approved source-version transition in Run lineage. Current read-result validation intentionally refuses changed files; it cannot simply be disabled after an edit. Subsequent external changes, deletion or revoked access must still prevent cached source re-exposure. Only after these boundaries are implemented and tested should the tool be registered and exposed to the desktop.
