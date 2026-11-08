# Single-file patch preparation — isolated implementation

This is the first TK-05/AG-007.02 component, not a registered executable tool. It performs no filesystem access, creates no approval and changes no files. The existing TK-06 creation permission cannot authorize editing.

`PatchArgs` contains a workspace-relative path, exact original SHA256 and a UTF-8 unified-hunk body. Unknown fields, protected paths, file headers, add/delete/rename directives and multiple target files are refused. The hunk body must end in LF. Context and removed lines match exactly; there is no fuzzy matching, offset guessing or line-ending normalization. CRLF and explicit no-final-newline markers are preserved. Empty replacement content does not delete a file.

Bounds are1 MiB for original/result text,64 KiB for the patch and128 hunks. NUL/binary/non-UTF8 source is rejected. Header ranges, overlap, output positions and old/new line counts are checked. The prepared result holds exact output bytes and a preview with input digest, old/new hashes, byte counts, line counts and complete displayed diff. This preview is a pure transformation of caller-provided bytes, not proof that a live file is authorized or unchanged.

Eight tests and strict all-target Runtime Clippy passed. Normal Agent dispatch, persistent edit consent/approval, publication, source lineage and Desktop integration remain unimplemented. This module must not be counted as complete apply_patch support.

## Execution work still required

The executor must read the real file under paired read/edit authority, bind the exact preview to the committed model response and tool contract, revalidate file identity and content before effects, and preserve both user and proposed versions on conflicts. A simple hash check followed by an unconditional overwriting rename is insufficient: another writer can change the target between those operations.

Publication must use reviewed target-platform primitives, keep durable recovery evidence, and never delete or overwrite an unrecognized competing version. Uncertain effects cannot be replayed. Do not advertise a global atomic content-CAS guarantee for uncooperative external filesystem writers without proof. Any capture/backup publication design must distinguish a clean conflict from a partially applied/unknown outcome and provide bounded recovery/retention behavior.

Successful edits also need an explicit approved source-version transition in Run lineage. Current read-result validation intentionally refuses changed files; it cannot simply be disabled after an edit. Subsequent external changes, deletion or revoked access must still prevent cached source re-exposure. Only after these boundaries are implemented and tested should the tool be registered and exposed to the desktop.

## Capability-relative source review

The Unix source-review component now exposes a distinct in-memory `EditFileGrant`, constructed by a trusted Host after separate edit consent. It cannot be deserialized or converted from a readonly/create grant. Preview requires a same-owner/policy/resource readonly grant as well. This is not yet persistent consent or an executable patch permission.

Real source bytes are read through no-follow directory/file capabilities with a1 MiB bound. Before/after descriptor and directory-entry identity, parent identity, creation/modification/change times, link count and mode are checked. Revalidation rejects a replacement object even when its bytes/hash match, and rejects changed permission metadata. The preview binding includes both edit and read environments. Leases remain held until the Host persists or discards the review; either revocation invalidates it, and drain waits for release. Preview still performs no file publication.

Thirteen patch/source-review tests and strict all-target Runtime Clippy passed on the available macOS filesystem, including actual source preservation, changed bytes/same-bytes replacement/mode change, symlink/hardlink refusal, different authorities, revocation and lease drainage. The source-review implementation is currently Unix-only; Windows target support, durable consent, review/approval storage, publication/recovery and lineage integration remain unfinished.

## Durable edit consent

Isolated SQLite schema8 now stores separate edit consents, immutable request keys and audit rows. Existing readonly/create-only records migrate without any editing rights. The consent binds owner, policy, physical resource, environment, version and Host epoch. The Host must reacquire OS access and explicitly restore a fresh capability after reopening; a transient in-memory review grant cannot be used as durable consent.

Revocation signals close live capabilities before durable acknowledgement. If persistence fails, the database remains unchanged but existing handles stay closed. Old request IDs cannot reactivate revoked rights. A changed physical directory invalidates its old consent; private namespaces cannot overlap active editors. Unresolved effects prevent fresh edit/create/read authorization for the affected resource. Limits are32 active and1000 historical edit consents with8192 request keys per owner.

The complete Runtime suite now passes118 tests and strict all-target Clippy, including actual schema7→8 migration, immutable history, revocation rollback, independent read/create rights, current source review and unknown-effect quarantine. Schema8 has only been applied to owned test databases. Per-operation patch preview/approval records, actual publication, recovery and Agent/Desktop exposure remain unfinished.
