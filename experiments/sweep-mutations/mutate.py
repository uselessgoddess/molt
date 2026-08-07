#!/usr/bin/env python3
"""Do the seeded sweeps bite?

A sweep that passes proves nothing on its own: a sweep that generates nothing
passes too. Each mutation below puts a bug back where one of them is supposed to
find it, runs only that sweep, and expects a failure. The tree is restored
either way.

    python3 experiments/sweep-mutations/mutate.py
"""

import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]

# (what the bug is, file, cargo test args, text to replace, replacement)
MUTATIONS = [
    (
        "release drops the extent it refused",
        "crates/molt-arch/tests/va_noise.rs",
        ["-p", "molt-arch", "--test", "va_noise"],
        "refused += 1;\n                        held.push(extent);",
        "refused += 1;\n                        drop(extent);",
    ),
    (
        "cover cuts a record before it knows it can finish",
        "crates/molt-arch/src/refcount.rs",
        ["-p", "molt-arch", "--test", "refcount_noise"],
        "if self.len + cuts > self.runs.len() {",
        "if cuts == usize::MAX {",
    ),
    (
        "the ring takes the tail the producer claims",
        "crates/molt-abi/src/ring.rs",
        ["-p", "molt-abi", "--test", "noise"],
        "if ready as usize > N {\n            self.fault = Some(Fault::Tail);\n"
        "            return Err(Fault::Tail);\n        }\n",
        "",
    ),
    (
        "the completion ring holds one more than it has",
        "crates/molt-abi/src/ring.rs",
        ["-p", "molt-abi", "--test", "noise"],
        "wrapping_sub(taken) as usize >= N",
        "wrapping_sub(taken) as usize > N",
    ),
    (
        "a round begins on what the last one flushed",
        "crates/molt-arch/src/shootdown.rs",
        ["-p", "molt-arch", "--test", "shootdown_noise"],
        "self.asked = asked;\n        self.flushed = 0;",
        "self.asked = asked;",
    ),
]


def run(name, path, args, old, new):
    source = ROOT / path
    before = source.read_text()
    if old not in before:
        print(f"stale  {name}: {path} no longer says what this mutation edits")
        return False

    source.write_text(before.replace(old, new, 1))
    try:
        caught = subprocess.run(
            ["cargo", "test", "-q", *args],
            cwd=ROOT,
            capture_output=True,
        ).returncode
    finally:
        source.write_text(before)

    print(f"{'caught' if caught else 'LIVED '} {name}")
    return bool(caught)


def main():
    ok = all([run(*mutation) for mutation in MUTATIONS])
    print("every mutation caught" if ok else "a mutation survived its sweep")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
