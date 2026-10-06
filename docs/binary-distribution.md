# Native CLI and MCP candidates

The `Native binary candidates` workflow prepares a CLI and MCP ZIP for each
explicit native runner: Windows x64 (`windows-2022`), Linux x64
(`ubuntu-22.04`, GNU target), and macOS arm64 (`macos-15`). These are unsigned
verification candidates. A workflow definition or locally verified Windows
archive does not establish that the other hosted jobs passed. Publication
remains maintainer controlled; this workflow has read-only repository access
and does not create releases or publish packages.

Names include both source versions, the full source revision and platform:
`rxls-0.1.4-mcp-0.1.0-candidate-<40-character-sha>-windows-x64.zip`.
The candidate is distinct from the published `v0.1.4` artifact. Its internal
`candidate.json` records a prepared archive. A separate verification receipt
records successful local or hosted execution, with the archive SHA-256 and
the actual execution context. Review the matching hosted run before treating
a receipt's hosted context as CI evidence.

Each archive contains `bin/rxls` and `bin/rxls-mcp` (with `.exe` on Windows),
usage instructions, both project licenses, the third-party license summary,
the complete MCP third-party notice and candidate metadata. Keep the notices
with the binaries. CLI dependencies are checked against the conservative
locked MCP notice using the existing production dependency graph. No runtime
Rust toolchain is required to run these unpacked binaries.

The CLI is compiled with Rust 1.85.0 and `full` plus its default CLI feature;
MCP is compiled with Rust 1.88.0. Both use release mode, locked dependencies,
explicit native targets and separate target directories. The source must be
an exact clean committed checkout, including nonignored untracked files.
Compiler host and actual operating system/CPU must match the selected target.
The manifest records source tree/archive identities, both manifest/lock
hashes, toolchain versions, runner image and observed host details. GNU/Linux
glibc compatibility and macOS compatibility require testing on the intended
deployment host; these candidates do not claim universal host compatibility.

The checker validates the externally supplied revision, platform and archive
checksum before extraction. It rejects traversal, unsafe Windows paths,
links, encryption, duplicate/case-colliding paths, unexpected inventory or
modes, mismatched native executable headers and member hashes. Limits are
64 MiB compressed, 96 MiB expanded, 32 MiB per binary, 512 KiB per legal file,
32 KiB metadata and 16 indexed members; the expected inventory is eight files.
Extraction uses a fresh owned directory.

The unpacked smoke verifies exact product versions, help, CLI CSV output and
actual newline-delimited MCP initialize/list/open/read/edit with recalculation/
save-copy/reopen/read/close. The tiny authored OOXML input has `A1=7` and
`B1=A1*2`, cached as 14. Editing A1 to 9 must yield a Computed cache of 18,
preserve the original input and preserve unrelated ZIP part bytes. This is
bounded distribution smoke, not general spreadsheet fidelity verification.
MCP `serverInfo` describes the SDK; product identity comes from `--version`
and the recorded binary hashes. Protocol/output limits, ten-second steps and
owned-process exit checks prevent an incomplete handshake from passing.
Build command timeouts/output overflow terminate the owned POSIX session or
Windows job, including inherited Cargo/compiler children. Windows assigns a
gated command worker before it may launch Cargo; assignment failure stops the
worker. Primary smoke failures, cleanup failures and supervised exit codes are
recorded separately. The Windows job path requires actual native validation;
passing pure child mocks alone does not establish that gate.

For an accepted committed source revision, on the matching native host:

```sh
python -m unittest discover -s scripts -p "test_binary_*.py"
python scripts/check_workflow_policy.py
python scripts/binary_candidate.py build --expected-sha <full-sha> --platform windows-x64 --output-dir target/binary-candidate-attempt01
```

Install the two pinned Rust toolchains first. Use a fresh exact clone or worktree rather
than deleting user files or modifying an active checkout to satisfy the
clean-source requirement. The output directory must be fresh and below
`target/` or `local/`. Preserve failure logs and use a new directory for each
retry. Verification alone can inspect a downloaded candidate without
executing it:

```sh
python scripts/binary_candidate.py verify <candidate.zip> --expected-sha <full-sha> --platform windows-x64 --expected-sha256 <trusted-sha256>
```

Archive preparation, local verification, all three hosted native jobs and
publication are separate milestones. No hosted result or publication is
implied by adding these scripts.
