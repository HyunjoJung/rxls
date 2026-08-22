# LibreOffice render-oracle container

This directory contains the reproducible Linux/amd64 LibreOffice 26.2.3.2
oracle image definition and its runtime security profile. The host wrapper is
`scripts/run-render-oracle-container.py` and uses only the Python standard
library.

## Prerequisites

- Docker with Buildx for canonical image builds. Docker or Podman may run an
  already verified Linux image.
- Enough local storage for the 216,816,909-byte official TDF archive and the
  built image.
- The checked-in render font pack acquired under `local/`.

Before the first hosted identity bootstrap, verify the source/build contract
and local assets explicitly in bootstrap mode:

```sh
python3 scripts/run-render-oracle-container.py verify-lock --bootstrap-identities
```

That mode is deliberately non-accepting: it records that the built config and
manifest digests are still missing. After a trusted hosted build emits two
matching isolated-build identities, select the artifact by exact workflow run,
source SHA, run attempt, and artifact ID; verify its GitHub artifact digest and
that the job reached the deliberate bootstrap failure. Download its
`render-oracle-image-build.json`, then emit a separate candidate lock:

```sh
python3 scripts/run-render-oracle-container.py pin-image \
  --build-evidence target/render-oracle-image-build.json \
  --github-run-id RUN_ID \
  --github-run-attempt RUN_ATTEMPT \
  --github-job-id JOB_ID \
  --github-artifact-id ARTIFACT_ID \
  --output-lock scripts/render-oracle-container/lock.pinned.json
python3 scripts/run-render-oracle-container.py \
  --lock scripts/render-oracle-container/lock.pinned.json \
  verify-lock
git diff --no-index \
  scripts/render-oracle-container/lock.json \
  scripts/render-oracle-container/lock.pinned.json
```

Only after reviewing both pinned digests, the bootstrap source commit, and the
diff should the candidate atomically replace `lock.json`. Never redirect
`pin-image` output over the input lock: the shell truncates a redirection target
before the wrapper can validate it. Normal campaign gates fail closed until the
reviewed config and manifest identities exist.

Acquire and verify the OFL-only font pack. It contains pinned metric-compatible
Latin faces (Carlito, Arimo, Tinos, Cousine, and Caladea), explicit Office font
aliases, and the existing Noto CJK/Arabic/Hebrew fallback faces:

```sh
python3 scripts/fetch-render-fonts.py --acquire
python3 scripts/fetch-render-fonts.py --verify
```

## Build

Inspect the exact build command without invoking a container engine:

```sh
python3 scripts/run-render-oracle-container.py build \
  --engine docker \
  --image rxls-render-oracle:lo-26.2.3 \
  --dry-run
```

Build twice in independent pinned BuildKit builders. Each build streams an
explicit Docker-schema2 archive through a 4 GiB stdout cap, hashes the exact
image-config blob, loads the archive explicitly, and then compares the complete
config/manifest/descriptor/RootFS identities:

The pinned BuildKit daemon uses its `native` worker snapshotter. The hosted
runner's overlayfs/containerd layer implementation is not part of the image
build contract because it produced different RootFS identities across runner
generations. The wrapper, workflows, and lock must agree on this snapshotter;
the policy gate rejects an `overlayfs` reintroduction.

```sh
python3 scripts/run-render-oracle-container.py build \
  --engine docker \
  --image rxls-render-oracle:lo-26.2.3 \
  --execute
```

Canonical builds require Docker. Podman remains supported for the isolated
`render` command after the image has been built and verified. An unpinned lock
is permitted only for the one-time hosted identity bootstrap. Normal builds
require the reviewed config and manifest digests and reject any different
result.

Executed builds and pinning additionally require a clean Git tree. The lock,
wrapper, Containerfile, entrypoint, and profile must all be tracked and
byte-identical to the recorded source commit.

## Render one workbook

Preflight the complete create/start/cleanup command plan without requiring an
installed engine:

```sh
python3 scripts/run-render-oracle-container.py render \
  --engine docker \
  --image rxls-render-oracle:lo-26.2.3 \
  --source tests/fixtures/xls/korean-unicode-biff8.xls \
  --font-pack local/render-fonts/pack \
  --evidence-dir local/render-evidence/container-korean \
  --run-id korean-smoke \
  --dry-run
```

Execute the same render by replacing `--dry-run` with `--execute`. The
evidence directory must be absent or empty. A successful execution writes:

- `oracle.pdf`: the LibreOffice `SinglePageSheets` export;
- `oracle-manifest.json`: path-neutral source and artifact identities; and
- `execution.json`: the verified image ID, enforced limits, and isolation
  contract.

`--corpus DIR` optionally adds a read-only corpus mount. It is not needed when
rendering a standalone source file.

## Runtime isolation

The wrapper always creates an ephemeral container with:

- no network, a read-only root filesystem, all capabilities dropped, and
  `no-new-privileges`;
- read-only source, font-pack, and corpus mounts;
- a size-capped writable evidence tmpfs and separate bounded runtime/tmp
  tmpfs mounts;
- fixed PIDs, CPU quota, memory/swap, file-descriptor, file-size, output, and
  wall-time limits;
- a unique HOME, XDG directories, and LibreOffice profile for every run;
- macro and Python execution disabled, with automatic external-link updates
  disabled;
- active embedded OLE/DDE enabled only inside this isolated container because
  LibreOffice uses the same global switch for native Calc chart objects;
- pinned OOXML/ODF load-recalculation policy with OpenCL and threaded formula
  calculation disabled;
- process-group termination followed by forced container cleanup on timeout.

Evidence is streamed from the bounded tmpfs, validated before installation,
and rejected if it contains a host input path. The host evidence directory is
installed atomically only after all checks pass.

Direct, unsandboxed host diagnostics use the separate
`scripts/render-oracle-host-profile.xcu`, which disables active OLE/DDE content
and therefore does not provide chart-acceptance evidence. Acquired or otherwise
untrusted chart workbooks must run only through this container path.
