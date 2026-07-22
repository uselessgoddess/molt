# Style

These conventions cover judgments that formatting cannot settle. Automated
checks enforce the objective subset through `just pre`; review covers the rest.

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

Keep decisions, hardware constraints, invariants, measured numbers, and
non-obvious orderings. Delete narration, headings, restated control flow, and
comments that only expand a name. Tests need a comment only when the scenario
cannot be made clear in the test name and assertions.

Write in the present tense, as if the code had always worked this way. History,
resolved problems, and commented-out code belong in Git.

## Documentation

Public APIs start with a one-sentence summary. Add only information the name,
signature, and types do not provide: invariants, costs, failure conditions, or
caller obligations. Private items need rustdoc only when they carry a contract
that is not evident from the code.

```rust
/// Marks the task's poll complete, preserving any wake that arrived during it.
```

Document the type or module, not every obvious method. Put genuine design
rationale in the narrowest relevant module and keep it proportional to the
decision it preserves.

Every `unsafe fn` has a `# Safety` section stating what the caller must
guarantee. Every `unsafe` block has a `// SAFETY:` comment saying why that
guarantee holds. One sentence: the condition that makes the block sound, not a
tour of everything that could go wrong. Several conditions join with a
semicolon; they do not become several sentences. The comment is never optional
— that is the exception verbosity does not get to make.

## Tests

A test is documentation that fails. Make the name the claim:

```rust
fn race_keeps_wake()
fn waker_is_task_local()
fn padding_is_typed()
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

Do not spread `#[cfg]` across call sites. Push shared behavior behind a trait or
generic function in `molt-arch`; the platform crate should only select its
concrete type:

```rust
#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    molt_arch::panic_handler::<RiscV>(info)
}
```

A `cfg` that appears more than twice for the same reason is a missing
abstraction.

## Commits

Subject in the imperative, under 72 characters, `scope(sub): what changed`.

The body says what was wrong before. A commit that only describes the new code
is redundant with the diff; the reader can see the diff and cannot see the
reasoning. Numbers, if the change is a performance change.
