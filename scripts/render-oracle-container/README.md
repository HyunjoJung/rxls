# LibreOffice render-oracle container

This directory contains the reproducible Linux/amd64 LibreOffice 26.2.3.2
oracle image definition and its runtime security profile. The host wrapper is
`scripts/run-render-oracle-container.py` and uses only the Python standard
library.

## Prerequisites

- Docker or Podman with Linux container support.
- Enough local storage for the 216,816,909-byte official TDF archive and the
  built image.
- The checked-in render font pack acquired under `local/`.

Before the first hosted identity bootstrap, verify the source/build contract
and local assets explicitly in bootstrap mode:

```sh
python3 scripts/run-render-oracle-container.py verify-lock --bootstrap-identities
```

That mode is deliberately non-accepting: it records that the built image ID is
still missing. After a trusted hosted build emits bootstrap evidence, review
and pin the exact image identity:

```sh
python3 scripts/run-render-oracle-container.py pin-image \
  --build-evidence target/render-oracle-hosted/container-build.json
python3 scripts/run-render-oracle-container.py verify-lock
```

Normal campaign and release gates use the second command and fail closed until
the checked-in image identity exists.

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

Build and inspect the resulting content-addressed image ID:

```sh
python3 scripts/run-render-oracle-container.py build \
  --engine docker \
  --image rxls-render-oracle:lo-26.2.3 \
  --execute
```

Use `--engine podman` for Podman. An unpinned lock is permitted only for the
one-time hosted identity bootstrap. Normal builds require the reviewed,
checked-in image ID and reject any different engine result.

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
- macro, Python macro, OLE/DDE, and external-link update suppression; and
- pinned OOXML/ODF load-recalculation policy with OpenCL and threaded formula
  calculation disabled;
- process-group termination followed by forced container cleanup on timeout.

Evidence is streamed from the bounded tmpfs, validated before installation,
and rejected if it contains a host input path. The host evidence directory is
installed atomically only after all checks pass.
