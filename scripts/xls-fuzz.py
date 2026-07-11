#!/usr/bin/env python3
"""Mutation fuzzer for the rxls `.xls` parser — validates the panic-free claim.

Takes real `.xls` seed files, applies random byte mutations and truncations, and
runs the rxls `extract` example over each. A Rust panic exits with code 101 (or
an abort signal); any such exit is a robustness failure. Clean errors (exit 1)
and successful extractions (exit 0) are fine — the contract is "never crash on
untrusted input", not "extract everything".

Usage:
    python scripts/xls-fuzz.py --corpus local/xls-poc/xls_host \
        --bin local/xls-poc/rxls-extract.exe --seeds 40 --mutations 80

Deterministic: seeded PRNG (no wall-clock), so runs are reproducible.
"""
import argparse
import glob
import os
import random
import subprocess
import sys


def mutate(data: bytes, rng: random.Random) -> bytes:
    b = bytearray(data)
    mode = rng.random()
    if mode < 0.45 and b:  # flip a handful of random bytes
        for _ in range(rng.randint(1, 16)):
            b[rng.randrange(len(b))] = rng.randrange(256)
    elif mode < 0.75 and b:  # truncate
        b = b[: rng.randrange(len(b))]
    else:  # splice/zero a region
        if len(b) > 32:
            i = rng.randrange(len(b) - 16)
            n = rng.randrange(1, 16)
            for k in range(n):
                b[i + k] = 0
    return bytes(b)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--bin", required=True)
    ap.add_argument("--seeds", type=int, default=40)
    ap.add_argument("--mutations", type=int, default=80)
    args = ap.parse_args()

    binary = os.path.abspath(args.bin)
    if not os.path.exists(binary) and os.path.exists(binary + ".exe"):
        binary += ".exe"

    files = sorted(glob.glob(os.path.join(args.corpus, "*.xls")))[: args.seeds]
    rng = random.Random(0xC0FFEE)
    tmp = os.path.abspath("local/xls-poc/.fuzz.bin")
    runs = crashes = 0
    crash_samples = []
    for f in files:
        seed = open(f, "rb").read()
        for _ in range(args.mutations):
            with open(tmp, "wb") as out:
                out.write(mutate(seed, rng))
            rc = subprocess.run([binary, tmp], capture_output=True).returncode
            runs += 1
            # 101 = Rust panic; negative = killed by signal (abort/segfault).
            if rc == 101 or rc < 0:
                crashes += 1
                if len(crash_samples) < 5:
                    crash_samples.append((os.path.basename(f), rc))
    if os.path.exists(tmp):
        os.remove(tmp)
    print(f"fuzz runs: {runs}   crashes (panic/signal): {crashes}")
    if crashes:
        print("FAIL — crash samples:", crash_samples)
    else:
        print("PASS — no panic/crash on any mutated input")
    sys.exit(1 if crashes else 0)


if __name__ == "__main__":
    main()
