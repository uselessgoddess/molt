# Do the seeded sweeps bite?

Stage 5.0's red teaming added four sweeps under
[`docs/testing.md`](../../docs/testing.md#red-teaming-the-address-space). Two of
them found real bugs, which is its own evidence. The other two found nothing,
and a sweep that finds nothing looks exactly like a sweep that generates
nothing.

So this puts the bugs back. Each mutation is one edit to one line, chosen to be
the defect the sweep is named for, and the sweep is expected to fail.

## Run

```
python3 experiments/sweep-mutations/mutate.py
```

It restores every file it edits, including on a failure.

## Result

```
caught release drops the extent it refused
caught cover cuts a record before it knows it can finish
caught the ring takes the tail the producer claims
caught the completion ring holds one more than it has
caught a round begins on what the last one flushed
every mutation caught
```

The first two are the bugs as they actually shipped, before `2cc7be1` and
`bc6e58a`. The last three are bugs the sweeps did not find because they were
never there — the point of running them is that the sweep would have.

The failures name what broke rather than which assertion fired: `Mega lost
address space` for the leaked extent, `a record in use covers addresses` for the
half-done refusal, and, for the shootdown, `no_run_of_flushes_leaves_an_address
_stuck_in_quarantine` — a tracker that stops does not crash, so a liveness sweep
is the only thing that can say so.
