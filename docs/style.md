# Style

Rules that rustfmt and clippy cannot check. Everything they can check is
already settled by `rustfmt.toml` and `just pre`; do not argue with them.

The theme is that Molt is read more than it is written, by one person who was
not in the room when the code was written. Short is a means, not the goal:
prefer the shortest form that still answers the next reader's question.

## Names

Short, and no shorter than the ambiguity allows.

- The module supplies the context. `Executor::wake`, not
  `Executor::wake_task`; `completion::Slot`, not `completion::CompletionSlot`.
- Reserve `new` for the obvious constructor. If there are two, name what
  differs: `reserve`, not `new_with_reservation`.
- No `get_` prefix, no `_impl` suffix, no Hungarian anything.
- Locals may be one word. `state`, `task`, `slot`. Loop indices are `index`,
  not `i`, unless the loop fits on one line.

## Comments

Say why. The code already says what.

```rust
// Mode bits [1:0] = 0 selects direct mode: every trap enters `base`.
unsafe { write_csr!(stvec, base & !0b11) }
```

That comment earns its line because `& !0b11` is a spec detail, not a fact
about Rust. A comment restating the expression would not.

Comment the decision, not the mechanism: a rejected alternative, a hardware
constraint, a measured number, an ordering that looks stronger than needed.
When a comment is long, it is because the reasoning is long — see the module
doc on `executor.rs` for why the slot states are separate atomics. That is
allowed. Filler is not.

Delete commented-out code. Git has it.

## Documentation

One sentence first, on its own line, stating what the item is. Then, only if
the reader still needs it, the invariant, the cost, or the caller's obligation.

```rust
/// Marks the task's poll complete, preserving any wake that arrived during it.
```

Document the type or module, not every method on it. A method whose name and
signature already say everything needs no doc comment. Modules carry the
design rationale; that is where a paragraph belongs.

Every `unsafe fn` has a `# Safety` section stating what the caller must
guarantee. Every `unsafe` block has a `// SAFETY:` comment saying why that
guarantee holds here. No exceptions — this is the one place verbosity wins.

## Tests

A test is documentation that fails. Make the name the claim:

```rust
fn a_wake_racing_the_scan_is_not_lost()
fn waker_marks_only_its_own_task_ready()
fn padding_follows_the_feature()
```

Not `test_wake`, not `executor_test_2`. If the name does not read as a
sentence about behaviour, the test does not know what it is testing.

Keep the body to three beats — set up, act, assert — separated by blank lines,
and prefer under ten lines. Give the assertion a message when the failure would
otherwise be a bare `false`:

```rust
assert_eq!(executor.next_ready(), Some(second), "the woken task became ready");
assert_eq!(executor.next_ready(), None, "no other task was disturbed");
```

One property per test. Two assertions about the same property are fine; two
properties want two names.

No helper frameworks, no shared fixtures, no `setup()`. A test that needs a
paragraph of scaffolding is telling you the API needs the work, not the test.

## Structure

- One concept per module, one module per file. No `mod.rs`.
- Public API is the smallest thing that lets a caller do the job. Nothing is
  `pub` because it might be useful later.
- Errors are enums. No strings, no `Box<dyn Error>` — `no_std` and the caller
  both want to match.
- No allocation in `molt-core`. If a design needs it, the design is wrong for
  this layer.

## Platform differences

Do not spread `#[cfg]` across call sites. Push the difference behind a trait in
`molt-arch`, or behind one exported macro, and let the platform crate spend a
single unconditional line:

```rust
molt_arch::panic_handler!(RiscV);
```

A `cfg` that appears more than twice for the same reason is a missing
abstraction.

## Commits

Subject in the imperative, under 72 characters, `scope: what changed`.

The body says what was wrong before. A commit that only describes the new code
is redundant with the diff; the reader can see the diff and cannot see the
reasoning. Numbers, if the change is a performance change.
