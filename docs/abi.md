# The user binary ABI

Status: Stage 5 decision record, August 2026.

Every cell Molt runs today was compiled together with the kernel. That is the
source of the whole guarantee: [`docs/architecture.md`](architecture.md) says
Molt gives up the hardware boundaries Redox keeps, and takes ownership,
lifetimes, and typed capabilities in exchange. A binary the compiler never saw
has none of that. Admitting one is not a new feature on top of the design; it is
the first thing that can break it.

This document decides how a binary is admitted — what isolates it, what the
bytes crossing the boundary look like, and what a program is allowed to ask for
— and it decides those in that order, because the isolation mechanism
constrains the ABI more than the ABI constrains anything else.

## What is being decided

1. The isolation mechanism for code the compiler did not check: **LFI**.
2. The image format and the descriptors that cross the boundary: **Molt's own,
   `repr(C)`, versioned, in a new `molt-abi` crate** — because LFI explicitly
   does not provide one.
3. The kernel-call mechanism: **the ring pair Molt already has**, with a
   four-entry runtime call table as the doorbell, and no syscall number space
   at all.
4. Inter-cell IPC: **a channel is a ring between two sandboxes**, set up by the
   kernel and then used without it.

Not decided here: the toolchain that produces such a binary, and what happens to
coreutils and a shell. That is [`docs/userspace.md`](userspace.md).

## Why LFI

The candidates, judged against Molt's constraints rather than in general.

| Mechanism | Boundary | Cost | Why not |
| --- | --- | --- | --- |
| Page-table processes | MMU + privilege transition | a syscall per call, copies or pinning at every buffer | Abandons the single address space, which is the thesis Molt exists to test |
| WebAssembly | Bytecode + validator | JIT or interpreter in the kernel; a second object model | The kernel would have to grow a compiler, and WASI is the exact "POSIX slop" this design is trying not to become |
| eBPF-style verification | Verifier with strong restrictions | Bounded loops, no general memory | A general program cannot be expressed, so it is not a userspace |
| **LFI** | Compiler emits a pattern, standalone verifier checks it | ~7–8% (loads and writes) or 1.5–6% (writes only), SPEC 2017 geomean; sandbox switch "10s of cycles" | Chosen |

LFI ([lfi-project/lfi](https://github.com/lfi-project/lfi)) sandboxes native
code in-process. The compiler emits a restricted instruction pattern; a
standalone verifier accepts or rejects the machine code; the runtime maps memory
so that any address the pattern can produce lands inside one 4 GiB window. There
is no privilege level, no address-space switch, and no trap.

That is the same shape as Molt's own argument — a static property, checked
before the code runs, replacing a runtime boundary — applied to code the Rust
compiler cannot check. It is the only candidate that adds an isolation
mechanism without adding an execution mechanism.

The overhead and sandbox-count figures above are the project's own, quoted from
its [README](https://github.com/lfi-project/lfi): "around 7% (Arm64) or 8%
(x86-64) overhead compared to native code when sandboxing reads and writes, and
1.5% (Arm64) or 6% (x86-64) overhead when only sandboxing writes (geomean on
SPEC 2017)".

## What LFI actually specifies

Read from the
[specification](https://github.com/lfi-project/lfi-specification), because the
details decide the ABI.

**The sandbox.** "The sandbox is a 4GiB region of memory, starting at a non-zero
4GiB-aligned address" (`x64/runtime.tex`). The 40 GiB before it and the 40 GiB
after it must be unmapped, and address 0 must be unmapped. Those guards are what
make an out-of-range address fault instead of hitting a neighbour.

**W^X is a rule of the scheme, not a convention.** "A page may only be readable,
read-writable, read-executable, or unmapped", and "if a page is executable, its
memory contents must pass the LFI-x64 verifier". Molt already asserts the first
half of that on itself — `MOLT_WX_OK` comes from
`Platform::verify_image_protection` — so the sandbox loader inherits an
invariant the kernel already knows how to check.

**Reserved registers.** On RISC-V (`riscv/verifier.tex`): `x21` holds the
sandbox base and nothing may write it; `x18`, `ra`, and `sp` may only be written
by `add.uw <reg>, xN, x21`; no instruction may cross an 8-byte-aligned address;
the Zba extension is required, because `add.uw` is what makes the scheme work.
On x86-64 the rewriter reserves `%r14` and `%r11`.

That `add.uw` is worth pausing on. It zero-extends the low 32 bits of a register
and adds the 4 GiB-aligned base. **The 4 GiB is not a policy choice — it is the
width of a zero-extension.** Any in-sandbox pointer is, by construction, a
32-bit offset from a base the code cannot change.

**Runtime calls, not syscalls.** "The first page of the sandbox must contain 256
runtime call pointers, each 8 bytes in length", and each may be a valid
entrypoint or zero. A runtime call transfers control with the return address in
a designated register, which "the runtime should treat as *unsanitized*, and
should only return to it if it is within the sandbox". So the kernel-facing
surface of a sandbox is a table it writes itself, of a size it chooses, with 252
entries it is free to leave zero.

**Portability is a non-goal of LFI, and so is a binary format.** From the
README: "Non-goals include a stable binary format and platform independence."
This is the single most important fact in this document. It means Molt cannot
adopt an LFI ABI, because there is not one. It must define its own — which is
what the issue asked for anyway — and use LFI purely as the enforcement
mechanism underneath it.

**Architectures.** `lfi-rewrite` "supports x86-64, Arm64, and riscv64"; the
runtime targets Linux on Arm64 and x86-64 with RISC-V "in-progress"; the LLVM
work lives in [a fork](https://github.com/lfi-project/llvm-project), not
upstream. Molt needs neither runtime — it is the runtime — but it does need the
verifier and the code pattern, and both exist for both of Molt's targets.

## The address-space budget, which is the real limit

The issue's concern is the 4 GiB. The 4 GiB is not the limit that binds.

Each sandbox costs 4 GiB of mapped window plus 80 GiB of guard: 44 GiB of
address space apiece, counting one guard per neighbour. The spec draws the
conclusion for Linux: "up to 2,977 sandboxes … within a standard 47-bit x86-64
userspace". Molt has no user/kernel split, so on x86_64 the figure is the same
order and irrelevant in practice.

On riscv64 it is not irrelevant. `crates/platforms/riscv/src/paging.rs`
implements Sv39 and only Sv39 — `SATP_MODE_SV39: u64 = 8 << 60` — which is
512 GiB of address space in total, and Molt's device windows are placed at
128 GiB. **Five sandboxes, give or take, is what Sv39 has room for**, and fewer
below the device window.

So the honest statement of the limit is: *4 GiB per program is not a constraint
for a teaching OS whose heap donation is measured in mebibytes; 44 GiB of
reserved address space per program is a constraint, and on Molt's RISC-V port it
binds at single digits.* Three things could move it, in increasing order of
work:

1. Implement Sv48 in the RISC-V paging module. 256 TiB puts riscv64 in the same
   bracket as x86_64. This is the fix, and it is Molt's own code.
2. `lfi-rewrite --p2size=variable` exists alongside `--p2size=32`, so a
   different power-of-two window is something the toolchain contemplates. What
   it costs in emitted code is not documented in the README and would have to be
   measured before relying on it.
3. Unmap idle sandboxes and reclaim their address space. Cheap to say, and it
   makes sandbox count a scheduling problem rather than a static one.

None of that is urgent. A design that supports five concurrent user programs on
one port is not a design that has failed; it is one that should say five out
loud, which the roadmap entry below does.

## The image and its descriptors

A new crate, `molt-abi`, `no_std`, no dependencies, compiled into both the
kernel and the user program. Everything in it is `#[repr(C)]`, and it is the
only place where a layout crosses the boundary.

```rust
/// The first bytes of every Molt user image.
#[repr(C)]
pub struct Header {
    /// `*b"MOLTABI\0"`, so a wrong file is a wrong file and not a fault.
    pub magic: u64,
    /// The ABI version this image was built against.
    pub version: Version,
    /// `size_of::<Header>()`, so a version that grew the header is caught
    /// by the loader rather than by the first field that moved.
    pub header_bytes: u32,
    /// Machine this image's text was verified for.
    pub machine: Machine,
    /// Offsets from the sandbox base, all 32-bit because the aperture is.
    pub text: Region,
    pub rodata: Region,
    pub data: Region,
    pub bss: Region,
    /// Where the program starts, and where its rings live.
    pub entry: u32,
    pub rings: u32,
}
```

Four rules govern it, and they are the whole versioning story.

**Every crossing type carries its own size.** A descriptor is validated as
`bytes >= size_of::<T>() for the version claimed`, never as "the struct I
compiled against". A kernel that has grown a field can still read an older
image; an image built against a newer ABI than the kernel implements is
rejected at load, by name, with the version it wanted.

**Layout is asserted, not assumed.** `molt-abi` carries host tests that check
`size_of`, `align_of`, and `offset_of!` for every crossing type against
hardcoded numbers. Changing a field is then a failing test with a diff, not a
field-shear bug that appears as garbage three layers away. This is the same
trick `xtask`'s markers play: make the invariant something a test can name.

**No Rust-layout types cross.** No `enum` with a payload, no `Option<&T>`, no
slice, no `bool` in a bitfield. Discriminants are explicit `u32`s with a
reserved zero. Padding is explicit and must be zero, so a future field cannot
collide with junk an old program left behind.

**Offsets, never pointers.** Everything the program names is a `u32` offset from
the sandbox base, because the aperture is 4 GiB and a 32-bit offset is exactly
its width. Validation is one unsigned compare — `offset + len <= 4 GiB`,
checked in `u64` so it cannot wrap — and it costs nothing to do it on every
access. The size the issue worried about is the same fact that makes the
boundary check a single instruction.

## The kernel call mechanism

**There are no syscalls.** Not renamed ones, not fast ones. The mechanism is the
one the kernel already uses for everything asynchronous: a
`IoRing<Op, Result, N>` submission/completion pair, from
[`docs/architecture.md`](architecture.md#ring-design), placed inside the
sandbox's own memory. The program fills a submission slot and publishes the tail
with `Release`; the kernel's driver end observes it with `Acquire`, does the
work, and publishes a completion carrying the same `RequestId`.

The runtime call table has four non-zero entries out of the 256 the spec
provides:

| Index | Entry | Why it cannot be a ring message |
| --- | --- | --- |
| 0 | `notify()` | Says "the submission queue was empty and now is not". Pure doorbell; a message announcing itself would need a doorbell. |
| 1 | `wait(deadline)` | The program has nothing to do. Parking is the kernel's job, and a ring cannot express "stop running me". |
| 2 | `exit(status)` | There is no later completion to deliver. |
| 3 | `fault(kind, pc)` | The scheme's own error path, reached when the sandbox traps. |

Everything else — every read, write, open, send, timer, and message — is a
submission. The rest of the table stays zero, and the loader writes zeros
deliberately rather than leaving them uninitialised.

This is the structural answer to "must not degenerate into POSIX syscall slop".
POSIX rots because it has a number space: an entry point is cheap to add, costs
nobody anything visible, and `ioctl` exists as a place to put what does not fit.
Molt's boundary has no number space to grow into. Adding an operation means
adding a variant to a versioned `repr(C)` enum in `molt-abi`, which changes the
version, which every program and the loader can see. Three further rules keep it
honest:

- **Every operation names a capability.** There is no ambient authority to act
  on, so no operation can be "do a thing to whatever the program means".
- **There is no escape hatch.** No `ioctl`, no `fcntl`, no opaque command word.
  If an operation cannot be expressed as a typed variant, it is not added.
- **An operation is added when a cell cannot be written without it**, and the
  commit that adds it names the cell. That is a social rule, but writing it down
  is what makes deleting an unused one an ordinary change.

The starting set, which is deliberately small:

```rust
#[repr(u32)]
pub enum Op {
    Read   { cap: Handle, offset: u64, buf: Region } = 1,
    Write  { cap: Handle, offset: u64, buf: Region } = 2,
    Flush  { cap: Handle }                           = 3,
    Open   { dir: Handle, name: Region }             = 4,
    Close  { cap: Handle }                           = 5,
    Send   { cap: Handle, buf: Region }              = 6,
    Recv   { cap: Handle, buf: Region }              = 7,
    Timer  { ticks: u64 }                            = 8,
    Message{ channel: Handle, buf: Region }          = 9,
    Grant  { channel: Handle, cap: Handle, rights: u32 } = 10,
}
```

(Written as a Rust enum for readability; the wire form is an explicit `u32` tag
followed by a `repr(C)` union with fixed-size arms, so the layout is stated
rather than derived.)

## Capabilities across the boundary

`molt-core`'s `Capability<R>` is an index and a generation packed into a `u64`,
and inside the kernel it is unforgeable because safe code cannot construct one.
A sandboxed program can write any `u64` it likes. So the rule at the boundary is
the one that was always true and has never had to be said:

**The integer is not the authority. The table is.**

A `Handle` in a submission is looked up in *that cell's own* capability table.
An index past the end is a range error; a generation that does not match is
`CapabilityError::Stale`, which is exactly what `CapabilityTable::get` already
returns for a handle whose owner was restarted. Guessing another cell's handle
does not help, because the other cell's handles are not in this table. Rights
are checked on the way through: a `Write` against a `Read` capability fails the
same way it fails in-kernel today.

That the same value means different things in two tables is a feature: it means
authority cannot be transferred by copying a number. Transfer requires `Grant`,
which is a kernel operation precisely because the kernel is the only thing that
holds both tables.

## Buffers

A submission's `Region` is `{ offset: u32, len: u32 }` from the sandbox base.
The kernel validates it once and then holds a `&mut [u8]` straight into sandbox
memory. No copy, no pinning, no page walk — because the sandbox lives in the
kernel's own address space, which is the entire point of the design Molt gave
hardware isolation up for.

Between submission and completion the buffer is *lent*: the program must not
touch it. Nothing enforces that, and nothing needs to. A program that scribbles
on a lent buffer corrupts its own I/O and nothing else, because the kernel
re-validates every length on every access and never keeps a pointer past the
call. Containment is the property being claimed; determinism for a program that
breaks its own contract is not.

## IPC

The requirement in the issue is that IPC must not become a future limitation.
The three ways message passing usually does are worth naming, because each one
dictates a decision:

- **It routes everything through the kernel** (L4-style), so the kernel is on
  the critical path of communication it has no interest in.
- **It is synchronous**, so a call graph becomes a dependency graph and a slow
  peer becomes everyone's latency.
- **It copies**, so the cost is proportional to the data and the API grows
  shared-memory bolt-ons later.

The design follows from avoiding all three. **A channel is a ring pair between
two sandboxes.** Its frames are mapped into both apertures, at whatever offset
each side has room for. Creating one is a kernel operation — it costs a
capability on each side, and the kernel is what has the authority to map a frame
twice. *Using* one is not: after setup, a message is a slot write and a `Release`
store, with the kernel involved only when a side decides to block via `wait`.

Two consequences are worth fixing in the ABI now, because retrofitting either
one is the thing that would make IPC a limitation later:

**Payloads are self-relative.** The shared region sits at a different offset in
each aperture, so an absolute offset written by one side is meaningless to the
other. Everything inside a channel message is an offset from the channel base.
Saying this before the first channel exists costs a sentence; saying it after
costs every program.

**Data is fast, authority is slow, and the split is visible.** A message carries
bytes without the kernel. A capability moves only through `Grant`, which
translates an index in the sender's table into a fresh index and generation in
the receiver's. Fusing the two would mean either putting the kernel back on the
data path or letting a sandbox mint authority, and the whole design is the claim
that neither is necessary.

What is deliberately absent: no shared mutable state between sandboxes other
than the channel region, no signals, no synchronous call primitive. Replies are
correlated by `RequestId` and arrive out of order, which is the same contract
every other queue in this kernel has.

## Loading, in order

The device stack has an ordering rule — a device must not be able to initiate
DMA before its domain exists, which is why `kernel/src/isolation.rs` exists at
all. The sandbox has the same rule, one level up: **code must not be executable
before a verdict on it exists.**

1. Reserve a 4 GiB-aligned aperture with its guards unmapped.
2. Map the image's pages writable, never executable, and copy it in.
3. Run the verifier over the text. A rejection frees the aperture; the pages
   were never executable, so there is no window in which they were.
4. Flip text to read-execute and drop write. This is `Permissions`' existing
   W^X constructor, on sandbox pages instead of kernel ones.
5. Write the runtime call table into the read-only first page, and the ring
   headers into the region the header names.
6. Enter.

Teardown reverses it, and a fault does not need a new mechanism: an LFI fault is
a cell fault, and cell faults already have `Supervisor::restart`, which bumps
the generation and calls `RestartHooks::revoke_capabilities`. Every handle the
sandbox exported goes stale by the same code path that already makes a restarted
in-kernel cell's handles stale. That reuse is the argument that the sandbox is
being added to this design rather than beside it.

## The verifier is Molt's to write

`lfi-verifier` is a C library that builds with meson and documents
`lfiv_verify_arm64` and `lfiv_verify_x64`; its `src/` holds `arm64`, `riscv64`,
and `x64` directories, so the architecture Molt wants first is the one whose
verifier is not in the published API. Linking C into a `no_std` kernel would
also put a C toolchain in the path of `just pre` for every contributor. Three
options, and the recommendation is a sequence rather than a choice:

1. **Verify on the host, at build time.** `xtask` runs the upstream verifier and
   records a hash; the kernel checks the hash. Correct only while images are
   baked into the disk image, because it trusts the loader's inputs. Right for
   the first sandbox that ever runs.
2. **Write the verifier in Rust, in the kernel.** It is a decoder plus a
   per-instruction rule check, and the rules are short — the RISC-V chapter is a
   handful of `specitem`s: nothing writes `x21`; only `add.uw` writes `x18`,
   `ra`, `sp`; nothing crosses an 8-byte boundary. This is the answer, and it is
   the most Molt-shaped code in the whole plan: a safe function from bytes to
   accept/reject, testable on the host with no hardware at all.
3. **Vendor the C verifier.** The fallback if (2) proves harder than it reads.
   It buys upstream's test suite at the cost of the build.

Whichever runs, it runs against the same corpus: the upstream verifier's own
accept/reject cases, so "Molt's verifier agrees with the reference" is a test
and not an assertion.

## Staging

Each step is a marker, in the style the rest of the kernel is checked in.

| Step | Marker | What it proves |
| --- | --- | --- |
| `molt-abi` exists, layouts asserted | host tests | The descriptors are stable before anything depends on them |
| A verified `nop`-and-exit image loads and runs | `MOLT_SANDBOX_OK` | Aperture, guards, W^X ordering, entry, `exit` |
| A rejected image is rejected | `MOLT_SANDBOX_REJECT_OK` | Step 3 above happens before step 4, and a bad image never becomes executable |
| One `Read` through a capability completes | `MOLT_SANDBOX_RING_OK` | Rings, offsets, capability lookup, lending |
| A forged handle fails | `MOLT_SANDBOX_STALE_OK` | The integer is not the authority |
| Two sandboxes exchange a message | `MOLT_SANDBOX_IPC_OK` | Channels without the kernel on the data path |
| A faulting sandbox restarts | `MOLT_SANDBOX_RESTART_OK` | LFI faults reach `Supervisor::restart` |

**riscv64 first**, which is not the order anything else in this kernel was done
in. [`docs/userspace.md`](userspace.md) has the measurement behind it: a stock
rustc can hold back the registers LFI-RISCV reserves and cannot hold back the
ones LFI-x64 reserves, so riscv64 needs no compiler fork and x86_64 does. The
sandbox does not touch drivers, which is the only reason x86_64 is ahead, so the
two lines of work do not queue behind each other.

## What this does not solve

- **Preemption.** A sandbox that spins never yields, and Molt has no timer
  interrupt into sandbox code. `lfi-rewrite --meter=(branch|branch-resume|fp|timer)`
  suggests instrumented metering is possible; whether Molt wants to pay for it
  is a separate decision, and until it is made, user programs are cooperative
  and the roadmap should say so.
- **Verification of the verifier.** Step 2 above moves the trusted computing
  base into Molt's own code. That is an honest trade and should be recorded as
  one, not presented as a proof.
- **Real hardware.** None of this changes with silicon, which is the one
  pleasant thing about it — see [`docs/hardware.md`](hardware.md).
