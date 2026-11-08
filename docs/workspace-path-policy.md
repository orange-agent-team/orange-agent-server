# Protected Unicode path aliases

The workspace boundary previously matched credential names with ASCII-only lowercase. On the tested macOS filesystem, `.Kube/proof.txt`, `.ſſh/proof.txt`, `.ßh/proof.txt`, `.ẞh/proof.txt` and `client.Key` resolve to protected ASCII names. A baseline regression actually read an owned protected fixture through the Kelvin spelling, and directory enumeration returned Unicode-spelled protected entries.

Protected-name comparison now uses compatibility normalization and expanded case conversion. This comparison key is used only to match the existing ASCII protected names and suffixes; it never replaces the requested path, tool arguments, digest or returned filename. Ordinary Chinese, combining-character, Greek and emoji paths remain supported and retain their literal spelling. This is not a claim that arbitrary filesystem names are globally interchangeable or that all target operating systems have been certified.

The implementation promotes the already-locked `unicode-normalization=0.1.26` to a direct Runtime dependency. No resolved package/version/source/checksum identity changed. Existing traversal, no-follow, hard-link, bounded-output, permission and source-version checks remain in force.

The fix is shared by direct file reads, directory enumeration, filename search and text search. Runtime acceptance also rejects an entire model tool batch containing a protected alias before committing a response, operation or approval; reopening does not dispatch it again. The model transport in that test is explicit fixture data. Direct filesystem tests are real on the available macOS host; no user directory or credential was used.

Evidence: the unpatched Runtime failed two new protection tests while the ordinary Unicode case passed. The patched Runtime passed all59 tests and strict Clippy. A first recovery assertion was corrected to match the existing API's RecoveryRequired view rather than expecting an exception; the no-replay and zero-effect assertions remain. Independent producer/consumer release and formal source-bound checks must pass before treating this as an integrated fix.
