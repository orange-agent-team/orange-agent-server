# Workspace text search — TK-04

`search_text.v1` accepts an authorized directory `path`, nonempty `query` (at most256 UTF-8 bytes), optional filename `glob`, `limit` (1–200), `regular_expression` and `case_sensitive`. Literal, case-sensitive matching is the default. Globs have the TK-03 bounded *, ? and ** syntax. Unknown/null fields and invalid paths or patterns fail before dispatch.

Regex uses the already locked Rust regex1.13.1 source, with1MiB compiled-program/cache caps and nesting32. Lookaround and backreferences are unsupported. Matching is line-oriented: return the first nonempty match on each line, skip zero-width matches, and never match across line breaks. Regex syntax may specify its own flags. No external command, remote pattern, template evaluation or backtracking engine is used.

Results contain workspace-relative path, full-file SHA256,1-based decimal line and absolute UTF-8 byte start/end offsets (end exclusive), plus a bounded excerpt. The excerpt is at most1024 bytes with explicit truncation. Results are untrusted content. A subsequent read must check current grant and, when requested, the observed full-file hash again. Search is not an atomic snapshot of a changing tree.

The capability walker has TK-03 entry/depth/link/protected-path limits. At most200 candidate files are considered, with10MiB per file,32MiB total actual successful reads (including binary/invalid UTF-8), and64KiB encoded output. Oversize files identified before reading are skipped with truncation. Other partial-read errors fail the entire search rather than allowing unaccounted I/O followed by more reads. Binary/unsupported files are omitted explicitly. Reaching any budget reports incomplete coverage.

The shared Runtime response commit, explicit approval, current grant, dispatch record and durable result flow applies unchanged. Cancellation revokes the grant; blocked reads retain their capacity/permission lease until drain. There is no automatic tool replay or inferred authorization from search results.

`python3 scripts/check-text-search.py` covers literal/regex semantics, Unicode ranges and full-file hashes, unsupported regex/arguments/zero-width matches, binary byte accounting, protected paths/aliases, excerpt/result limits, prepared revocation and once-only approved result across SQLite reopen. Existing read/list/file-search tests cover the shared refactor. Tests are local filesystem/SQLite with scripted model replies. Desktop remains on its independently pinned Runtime; actual model and target OS certification are separate.

The isolated fixed CPU model has also completed an exact-scope `search_text` task: it selected the actual tool, approval preceded execution, the persisted full-file SHA/excerpt matched an unpredictable generated fixture, and reopening did not repeat effects. `check-real-agent.py --required-tool search_text` preserves these assertions and per-attempt reports. This is one development model/task, not broad model qualification.

The independent exact-filename `search_files` task failed scope adherence in three bounded development-model attempts. Approval was refused each time; the expected path/glob was not broadened to manufacture success. That model's filename-search qualification remains unresolved even though deterministic Rust/filesystem/SQLite tool tests pass. Further same-cause retries await upstream changes.
