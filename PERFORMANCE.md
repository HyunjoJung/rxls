# Performance and resource budgets

rxls treats the absolute performance and resource ceilings as automated
release-gate failures, not just benchmark observations. Run
`scripts/measure-performance.py` against the release
binary or the release-built edit/save example. It records machine-independent
input paths, input/output byte counts, repeated wall-clock samples, and peak RSS
in the stable `rxls.performance-evidence.v1` schema. Wall limits are hard
process-group timeouts. When an RSS ceiling is requested, an unavailable memory
sample fails the gate instead of silently skipping the ceiling. Every sample
uses the larger of the polling result and the platform child-rusage peak, so a
very short process cannot be under-reported when polling only observes its
pre-`exec` image.
For `diagnose`, `output_bytes` is the emitted diagnostic JSON size. For
`edit-save`, it is the saved workbook size; every sample records the value and
the case records whether repeated output sizes were consistent.

Release evidence uses these ceilings on the Linux release runner:

| Workload | Wall time | Peak RSS |
| --- | ---: | ---: |
| Tracked small fixture | 1 s | 128 MiB |
| Deterministic 16 MiB medium OOXML fixture | 5 s | 512 MiB |
| Largest pinned corpus workbook | 30 s | 1 GiB |
| Edit/save of the deterministic 16 MiB fixture | 10 s | 768 MiB |

The current deterministic inputs are 8,157 bytes for the tracked small fixture,
16,783,712 bytes for the generated medium package, and 4,746,305 bytes for the
largest pinned corpus workbook. The edit/save workload produces a consistent
12,755,614-byte package across repeated samples. These are release budget
fixtures, not a claim that every workbook up to an arbitrary file-size boundary
fits the same time or memory ceiling.

The native synchronous reader rejects or bounds oversized package parts, XML
work, text expansion, images, dimensions, repeated ODF cells, formula recursion,
and evaluation operations. The WASM binding has a separate 32 MiB synchronous
input limit and bundle-size budgets documented in the
[WASM package guide](https://github.com/HyunjoJung/rxls/blob/main/bindings/wasm/npm/README.md).

Local and hosted candidate measurements must pass the absolute ceilings above;
those ceilings are the release's performance regression guard. There is no
accepted comparable `0.1.1` timing/RSS baseline, so `0.1.2` does not claim a
measured percentage improvement or regression relative to `0.1.1`.

The second clean candidate treats the first run of the same commit as its
reproducibility reference, not as a historical performance baseline.
`scripts/compare_release_bundles.py` rejects a run-to-run median wall-time
increase above 20% once it also exceeds a 250 ms measurement-noise allowance,
peak RSS increase above 15% once it also exceeds a 16 MiB sampling-noise
allowance, or edit/save output-size increase above 10% on the same pinned input
and exact source commit. The absolute allowances prevent timer and RSS sampling
quantization and shared-runner scheduling from turning sub-second or sub-page
cases into false regressions; every run must still pass the stricter absolute
ceilings above.
The comparator also fails closed on missing, zero, non-finite, non-passing,
mismatched, timed-out, or incompletely sampled evidence; deterministic
non-measurement fields must remain identical.

Example:

```sh
cargo build --release --all-features --locked
python3 scripts/measure-performance.py \
  --operation diagnose \
  --case small=tests/fixtures/xlsx/reader-structural.xlsx \
  --repeat 3 --max-seconds 1 --max-rss-mib 128 \
  --output target/performance-small.json

python3 scripts/generate-performance-fixture.py \
  --output target/performance/medium.xlsx --payload-mib 16
cargo build --release --example edit_save_benchmark --all-features --locked
python3 scripts/measure-performance.py \
  --bin target/release/examples/edit_save_benchmark \
  --operation edit-save \
  --case medium-edit=target/performance/medium.xlsx \
  --repeat 3 --max-seconds 10 --max-rss-mib 768 \
  --output target/performance-edit.json
```
