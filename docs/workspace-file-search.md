# Workspace filename search — TK-03

`search_files.v1` extends the server-owned shared Runtime with capability-relative filename search. Inputs are `path` (workspace-relative directory, or `.`), `glob` and optional `limit` (1–200; default100). Glob `*` and `?` match within one component; `**` must occupy a whole component and matches zero or more directories. Character classes, braces, exclusions and escapes are rejected. No shell or regular-expression engine is invoked.

Results contain workspace-relative file paths, scanned entry count, `truncated` and `omitted_unsupported`. Protected components, symbolic links, nonregular files and Unix hard links are omitted. Traversal uses no-follow directory capabilities, never ambient paths. Root or descendant replacement may fail the search; it is not silently followed. Concurrent filesystem changes are not a transactional snapshot. A returned filename does not authorize a subsequent read.

Each operation scans at most10000 directory entries and32 levels, returns at most200 paths, and bounds encoded JSON to64KiB. Depth, entry, result-count or byte limits report truncation. Results are sorted within the inspected subset. There is no recursive cursor or claim that a truncated search covers all matching files. The existing10-second deadline,4-read concurrency limit, cancellation quarantine and grant drain rules still apply.

The existing response commit → intent → explicit approval → dispatch → result flow owns this tool. The model cannot grant itself access or approve execution. Current owner/environment/policy and revocation are rechecked by the same prepared-tool path as reads. SQLite recovery preserves the approved intent and publishes exactly one tool result.

`python3 scripts/check-file-search.py` runs actual filesystem and SQLite checks for matching/Unicode, argument rejection, sensitive directories/link aliases, scope and prepared revocation, depth/entry/JSON limits, explicit approval and reopening without duplicated result. The model transport is an explicit script. Broader Runtime tests and strict Clippy passed in preparation; formal source regression is required before this slice is DONE.

Desktop still consumes its independently pinned Runtime release; this source change does not silently upgrade its vendor bundle. Windows/Linux target certification, TK-04 text search and the remaining G3 tools are separate work.

A separate actual development-model exact-filename scenario is currently **not qualified**: three bounded attempts proposed a different glob or directory and were refused before approval. Deterministic tool/host checks are separate passing evidence. `check-real-agent.py --required-tool search_files` retains the exact-scope and persisted-result assertions for later model qualification; no fallback broadens the requested search.
