# MoltFS

Status: Stage 3 COW B-tree filesystem, on a block ring since Stage 4.4,
July 2026.

How the read-only Stage 2.4 image became a writable, crash-consistent
filesystem; how the bounded journal and copy-on-write metadata tree divide the
work; what comes from bcachefs; and how capabilities, block durability, caching,
and power-loss tests fit together. This is the record for `molt-block`,
`molt-fs`, and `molt-shell`.

## What this stage has to answer

Stage 2.3 ends with a sector reading back correct. That is a device, not
storage: nothing yet says which bytes mean something, nobody holds a reference
to a file, and the only consumer is a smoke test comparing a pattern. Stage 2.4
is where a name becomes a thing you can hold, and it has to answer four
questions before writing a byte of format:

1. **What is on the disk.** Superblock, objects, extents — the shape the next
   ten years of the filesystem grow out of, because a format is the one thing
   here that cannot be refactored without rewriting every image ever made.
2. **How a cell asks for a file.** Molt has no processes, no paths resolved by
   the kernel, and no `open(2)`. It has rings and capabilities, so the protocol
   has to be built out of those and be pleasant enough that nobody wants a
   shortcut around it.
3. **What survives a crash.** Stage 2.4 established dual superblocks; Stage 3
   has to turn that shape into a real ordered checkpoint and prove every power
   cut.
4. **Where the driver ends and the filesystem begins.** Stage 2.3 shipped
   `molt-virtio` with a `read` method on it. If a filesystem is written against
   that method, the second storage driver is a rewrite of the filesystem.

## Read-only, and why that is the interesting version

The instinct is to build the copy-on-write filesystem directly, because CoW is
where the design is going and a read-only format looks like a throwaway. It is
the opposite: a read-only volume is the whole read path, and the read path is
what every later feature is measured against.

Writing is where the hard parts are — allocation, a journal or a checkpoint
tree, ordering against a device that reorders, fsync semantics, and the tests
that cut power at every one of those points. None of it can be designed
honestly before there is a reader whose invariants it has to preserve. Building
the reader first means the write path arrives with something to be correct
*about*, and it means Stage 2.4 ships something that works instead of something
that half-works in two directions.

The cost was real and bounded: Stage 2.4 carried no allocator, log, free-space
map, or write path. It did establish the structures its successor needed:
dual generation-stamped superblocks, checksummed metadata, crc32c per data
block, and extents rather than block pointers. Stage 3 preserves that base and
adds the log banks around it.

## Taking from bcachefs rather than btrfs

The brief was btrfs's ideas without btrfs's legacy, leaning bcachefs. The ideas
arrive here in the cheapest form that is still the real thing.

**Checksums that cover data, not just metadata.** Every data block carries a
crc32c in a region of its own, and every metadata region carries one in the
superblock. This is bcachefs's position — checksums are not optional and not a
mount flag — and it is why [`Volume::mount`](../crates/molt-fs/src/volume.rs)
verifies all six regions before the first lookup rather than discovering
corruption at whatever block a directory search happens to land on. A volume
that mounts is a volume whose metadata is intact, which is a much stronger
statement than "the superblock parsed".

The sums live in their own region rather than beside the blocks they cover.
That costs a second block read per data block — or did, until `Volume` had
enough slots to keep the sums block resident while the file it covers streams
past — and it buys a scrub that walks one contiguous region instead of seeking
across the volume, the Stage 4 item this leaves room for.

**A generation in the superblock, and a checkpoint that swings it.** Below.

**Filesystem state as typed B-tree keys.** bcachefs treats the filesystem as a
database: metadata records are keys in a small set of B-trees, and its journal
records key updates which replay inserts back into those trees. MoltFS uses one
tree with three key spaces:

- `Object(id)` maps to current kind, entry count, and file size;
- `Dirent(parent, name)` maps a directory leaf name to an object id;
- `Write(object, cursor)` maps a file update to its journal payload.

This is the same useful boundary at smaller scale: namespace and object queries
are tree lookups rather than mutation-log scans, while file payloads remain in
the bounded log until extent allocation and compaction arrive. See bcachefs's
[architecture overview](https://bcachefs.org/) and
[transaction design](https://bcachefs.org/Transactions/).

**Extents, not block pointers.** A file is a run of `(logical, blocks, block)`
records, sorted by logical block and binary-searched. Contiguous data costs one
record however long it is, a logical block no extent covers is a hole that reads
as zeros, and `xtask mkfs` drops every all-zero block on the floor — so a sparse
file costs its content, not its length. Extents are also the only structure here
that a writable version would have kept anyway; block pointers would have been
thrown away.

What was deliberately *not* taken:

- **Reflinks, snapshots, subvolumes.** All three need reference-counted general
  allocation. The room they will take is a superblock field and a region, both
  of which the layout has space for.
- **Inodes as a namespace.** There is no `stat` on a number, no hard links, and
  no `.`/`..`. An object is reached by having opened it; see below.
- **btrfs's on-disk anything.** The item/key/leaf machinery, the chunk tree, and
  the backref format are solutions to problems Molt does not have yet, and each
  one is a compatibility obligation from the moment an image exists.

## The base format

Six regions, two superblocks, all little-endian, everything block-addressed.
[`layout.rs`](../crates/molt-fs/src/layout.rs) is the definition; both the
reader and `xtask mkfs` compile against it, so there is no second copy of the
format to drift.

```
block 0   superblock copy 0
block 1   superblock copy 1
          objects   one 32-byte record per object, indexed by id
          extents   16-byte runs, sorted by logical block within a file
          entries   16-byte directory entries, sorted by name within a directory
          names     the byte arena every entry's name points into
          sums      one crc32c per data block
          data      the blocks extents address
          tree      fixed arena of checksummed 4096-byte COW nodes
          log 0     active, previous, or free checkpoint bank
          log 1     active, previous, or free checkpoint bank
          log 2     active, previous, or free checkpoint bank
```

Blocks are 4096 bytes. Every record size divides the block size, so no record
straddles a boundary and a reader needs exactly one block of buffer to reach any
of them — which is why an object is 32 bytes rather than the 24 its fields use.
That constant is what keeps reading bounded: a mounted `Volume` holds exactly
one `[u8; 4096]`, boxed, and every record it parses is reachable from it.

**The superblock** carries a magic, version, block size, generation, volume
length, root object id, data geometry, tree arena and root, log-bank capacity,
and six region descriptors — each an offset, length, and crc32c. The sixth
descriptor names the complete mutation log for that checkpoint. Its own
checksum is checked before any field is trusted. `Super::check` also proves the
tree root lies inside its arena, the selected log lies at exactly one of three
bank boundaries, and base metadata, data, tree, and log banks do not overlap.

**An object** is a kind, a start index, a count, and a size. For a directory the
range is into the entries region and the size is zero; for a file the range is
into the extents region and the size is the file's length in bytes. One record
serves both because the difference between them is one byte, and a filesystem
that needs two record types before it has a write path has already spent its
simplicity budget.

**An entry** is a `(name_at, name_len, object)` triple pointing into the name
arena. Names are out of line so an entry stays 16 bytes and a directory search
reads one block per probe regardless of name length; `name_len` is a `u16` on
disk, and `MAX_NAME` — 255 — bounds only the copy a lookup makes onto the stack
and the inline `Name` a ring carries, not the stored form.

## Writable tree and payload log

The base image remains immutable. `Journal` appends two typed payload records:

- `Create(object, parent, kind, name)` allocates the next object id and adds one
  directory entry.
- `Write(object, offset, bytes)` overlays file data. Later records win, and a
  write beyond end creates a zero-filled hole.

Records start on 512-byte sector boundaries. One sector write can therefore
tear only the record being appended, never an earlier record. The active
superblock carries the exact log length and its crc32c, so padding and
uncommitted tail bytes are invisible.

Each mutation also inserts its current state into the metadata B+ tree. A node
is one checksummed 4096-byte block. Leaves hold typed keys and values; internal
nodes hold separator keys and child blocks. Insertion keeps a fixed
root-to-leaf path, writes a replacement path, splits full nodes, and returns a
new root. It never overwrites a node reachable from either durable
superblock. `Journal::sync` publishes that root only after the nodes and log
have passed a durability barrier.

The tree API is deliberately small: exact lookup, ordered successor, insert,
and transaction root. Filesystem code builds object, directory, and write keys
on top rather than teaching the tree about files. Nodes, the path a mutation
walks, and the split scratch live on the heap — see [the stack budget](#the-stack-budget)
for what that replaced.

The tree arena has a bounded tracing allocator. Starting a transaction marks
nodes reachable from the active and previous roots; every other arena block is
reclaimable. Replaced paths created in the same transaction are released
immediately. This keeps both crash fallbacks intact while allowing old
generations to be reused without fsck. `build_with_capacity` selects tree and
log capacity, and `FsError::Full` reports either finite bound explicitly.

## Metadata cache

`MetadataTree` keeps sixteen parsed nodes, about 64 KiB of heap, under a bounded
second-chance policy inspired by
[SIEVE](https://www.usenix.org/conference/nsdi24/presentation/zhang-yazhuo).
A hit sets one visited bit and does not move the node. The eviction hand clears
visited candidates and replaces the first unvisited one. This is useful here
because it has constant metadata, no linked allocation, and makes repeated
root and directory probes device-read free. Entries are `Rc<Node>`, so a node a
lookup is standing on survives its own eviction and the walk costs a refcount
rather than a copy of 4 KiB. `Journal::tree_stats` exposes hit, miss, and
eviction counters for tests and diagnostics.

**Why the tree caches, and not a page cache underneath it.** The question is
worth answering explicitly, because a page cache is the usual answer and it is
the wrong one to reach for first. What this cache stores is a *parsed* node —
header validated, checksum verified, keys addressable — so a hit skips the
decode as well as the read. A block cache under `Device` would store bytes, and
every hit above it would re-verify a checksum it already verified. The two are
also owned by different people: node lifetime follows the COW discipline
(a block reachable from a durable root is immutable, so a cached copy can never
go stale), while page lifetime follows dirty state and writeback, which is a
policy this filesystem does not have and does not want to inherit.

A page cache still belongs in the system, one layer down and later: it is what
makes data blocks and a second filesystem cheap, and it goes behind
`molt-block`, beside the `BlockOp` ring that is already there, where the SMP
story can be settled once for every client. Two caches then is not duplication —
the block cache holds bytes for whoever reads them, the metadata cache holds
structure for the tree that owns it. Data reads keep `Volume`'s slots until that
day, because a data cache without an eviction policy tied to writeback is the
half of a page cache that only looks free.

## The stack budget

The kernel stack is 128 KiB, one per core, and nothing grows it. The first
writable tree spent most of it: a mutation carried a root-to-leaf path of parsed
nodes, a split carried two more, and each node is 4 KiB, so `Journal::mount`
measured 78 912 bytes of stack and one `create` 98 304. On the host those frames
are free; on the kernel stack they are two operations from an overflow that has
no guard page under it.

`crates/molt-fs/tests/stack.rs` is the record. It paints a 96 KiB window in a
frame below the current one, runs a single mount or a single create/sync, and
reads back how far the paint was disturbed. Both budgets are 16 KiB and both
pass with room: in a debug build `Journal::mount` spends 10 264 bytes, a create
7 616, and a sync 2 464. The test fails loudly if a future change puts a node
back on the stack, which is the only way this property stays true.
`experiments/stack_probe.rs` prints the same measurement phase by phase.

What moved: nodes are built on the heap and cached as `Rc<Node>`; the mutation path is
a `Vec` of block numbers rather than of nodes, so descending costs 8 bytes a
level; the split scratch is one boxed block owned by the tree; the free-space
bitmap is a `Vec<u64>` sized from the arena; and `Volume` owns its 4 KiB block
buffer instead of borrowing one from every caller, which also erased a lifetime
from `Volume`, `Journal`, and `Fs`. The kernel donates 4 MiB of claimed frames to
`molt-alloc` at boot, which is where all of that now lives.

The trade is honest rather than free: a bounded array cannot fail, and a heap
can. `FsError::Full` still reports the arena and log bounds, and an exhausted
heap is `FsError::Memory` next to it — a filesystem that already answers errors
has no business taking the machine down over one node it could not get.
`crates/molt-fs/src/mem.rs` is where that mapping lives: `Box::try_new_zeroed`,
`Rc::try_new_zeroed`, and `try_reserve` behind names the call sites use, so no
allocation in the crate reaches `handle_alloc_error`. `mkfs` is the exception
and stays infallible — it runs on a host, behind a feature the kernel does not
enable.

`Rc::try_new_zeroed` rather than a `Box` converted afterwards, because a node is
built field by field and shared only once it reaches the cache: converting would
allocate the four kilobytes twice, and `Rc::try_new(node)` would build it on the
stack the budget above measures. `mem::Unique` is the window while the handle is
still the only one — it derefs mutably through `Rc::get_mut` and gives itself up
with `shared()`.

Bounding the filesystem's demand is still what keeps refusal rare: the cache is
sixteen nodes reserved at mount, a path is `MAX_HEIGHT` block numbers, and a
mutation allocates a replacement path, not a tree. `crates/molt-fs/tests/memory.rs`
is the proof it is handled rather than merely typed — it replaces the global
allocator with one that refuses large allocations on the calling thread, and
shows a mount answering `FsError::Memory` and a refused create rolling back to
its snapshot while the journal keeps taking work.

## Crash consistency

The invariant is exact: **after power loss, mount returns the complete old
generation or the complete new generation, never a mixture, and needs no
fsck.**

Two superblocks are not enough by themselves. If a new transaction overwrote
the previous generation's log while the active generation still depended on
it, a crash before the new superblock would destroy the fallback. MoltFS keeps
three log banks:

1. one named by the active superblock;
2. one named by the previous superblock;
3. one safe target for the next transaction.

The first mutation copies the active log into the free bank, appends there, and
writes new COW nodes into unprotected arena blocks. `Sync` uses one
deterministic, synchronous sequence:

1. finish all target-bank and COW-node writes;
2. issue device `flush`;
3. write the older superblock copy with generation + 1, target bank, length,
   checksum, and new tree root;
4. issue device `flush` again.

The first flush makes every byte the new superblock will name durable. The
second is the commit point. Losing power before it leaves both old
superblocks and their banks intact; losing power after it leaves a complete
new checkpoint. Mount parses both copies in generation order and verifies each
selected log. If the newest copy parses but its log checksum fails, mount
continues to the previous copy instead of treating a generation number as
proof. It applies the same rule to the tree: every reachable node checksum,
level, child address, and generation is verified before the checkpoint can win.

There is deliberately one outstanding mutation at a time. Reads go several deep
on the ring; a write, a flush, and a checkpoint each submit and await, so
ordering stays observable and deterministic — barriers separate durability
epochs, and nothing reorders requests within one. `molt-block::Fault` models
volatile controller cache separately from stable storage. The crash test starts
from generation 2, rotates into the third bank, and cuts power before every
record, tree-node, flush, and superblock action until a full checkpoint
succeeds. Every interrupted run remounts generation 2; the first uninterrupted
run remounts generation 3 with all bytes. Separate tests corrupt the newest log
and newest tree root and require fallback, and cycle hundreds of checkpoints to
prove arena reclamation. Those tests are the recovery algorithm, not a
simulation around it.

## Schemes: no, and here is the line

Redox's schemes are the strongest idea in its design: every resource is a URL,
`scheme:path`, resolved by a userspace daemon that owns the namespace, so a
filesystem, a network stack, and a display are the same kind of thing and none
of them is special to the kernel. Under a cytokernel the question is whether to
adopt them, and the answer is that Molt already has the half that matters and
should not adopt the other half.

**What schemes are actually solving.** In Redox a process starts with nothing
but its parent's file table and a way to *name* things it has never seen. A
string namespace is how an unprivileged process reaches a resource, and the
scheme daemon is where policy about that reaching lives. It is a good answer to
"how does an isolated process obtain authority" — for a system whose isolation
unit is a process and whose IPC is a file descriptor.

**Why it does not carry.** Molt has one address space and typed capabilities. A
cell does not obtain authority by naming it; it obtains it by being handed a
`Capability<Dir>`. Adding a string namespace on top means adding a resolver, and
a resolver is precisely a component that turns a name nobody vouched for into an
authority somebody has to check — which is the ambient-authority mistake the
capability model exists to avoid. It also costs a parser, an error type for
malformed names, and a place where two subsystems disagree about normalization,
all of which are Linux's `path_lookup` in miniature.

So there are **no paths in `FsOp`**. `Open` takes a `Capability<Dir>` and a
single `Name` — a leaf, checked to be non-empty, at most 255 bytes, and free of
separators. Walking is done by the client, one hop at a time, and each hop
returns a handle. A cell holding a capability to one subdirectory cannot address
anything outside it, which is what a chroot does elsewhere and what the type
does here, for free and without a jail to escape.

**What is worth keeping from schemes, and where it goes.** The valuable half is
not the string — it is that a filesystem, a socket, and a device are *the same
kind of endpoint*, so a client written against one shape talks to all of them.
Molt spells that shape as a ring of typed operations plus capability handles,
which is what `FsOp`/`FsDone` is and what a future `NetOp` will be. When Stage 3
needs discovery — "which service answers for storage" — that is a registry of
capabilities, not a URL namespace, and the roadmap already lists it as *a typed
scheme/resource namespace*, emphasis on typed. This document is the record that
"typed" means capabilities, not strings.

**What discovery turned out to be.** `molt_core::registry` is that registry, and
it is smaller than the sentence above suggested. A `Scheme` is a type with an
`Endpoint` type beside it — `Storage`, whose endpoint is a `Mount` carrying a
`Capability<Dir>` root — and the registry is a `CapabilityTable` of endpoints
with the scheme as the marker. `publish` takes an endpoint and returns nothing a
client can use; `acquire` returns a `Capability<S>` lease; `endpoint` exchanges a
lease for `&S::Endpoint`. There is no name to spell, no lookup that can be
misspelled, and one scheme per registry slot, so "which service answers for
storage" is answered by the type system and the miss is `RegistryError::Unavailable`
rather than a path that does not resolve.

The lease is the part that earns its existence. A client that took the `Mount`
by value would hold a root capability across a restart of the thing that minted
it, and would find out only when a handle came back `Stale` — three operations
later, from the middle of a command. Naming the *publication* instead means the
service's restart hooks withdraw it, so the next `endpoint` call fails at the
first hop with `CapabilityError::Stale` and the client re-acquires there. That is
what [`molt_fs::Teardown`](../crates/molt-fs/src/restart.rs) does between
`cancel_requests` and the remount, and what `Fs::publish` restores afterwards.
The mount is published for the service's own `CellId`, so revoking a *client*
cannot take the publication down with it.

## Cells: the filesystem is a service now

The sketch in the issue had `AppCell → FsCell → VirtioCell`, three cells and two
rings. The read-only stage shipped one ring and no cell, because the restart
story for a mount needs a write path to be worth writing. The write path exists,
so `molt_fs::FsCell` does too: init starts one, it publishes the only mount in
the system, and the shell is a cell beside it rather than a task init drives.

**What it is.** `FsCell::spawn` mounts the device and keeps it; `fs()` hands out
the `Fs` other cells submit to; `Supervisor` runs the lifecycle around both. A
restart stops submissions, cancels in-flight requests, revokes every capability,
and remounts — in that order, because a restart that let one more submission in
would answer it from a filesystem that is half gone. The remount goes back to
the last durable checkpoint, so what was synced survives and what was not is
exactly what a power cut would have taken. The supervisor's `generation()`
counts restarts, `checkpoint()` reports the volume's, and the two are
deliberately different numbers.

**When the remount fails.** By then the hooks have run: handles are revoked, the
tree is dropped, and the log is unreplayed, so there is no filesystem left to
answer with. The cell says so rather than pretending — `health()` turns
`Health::Failed` and every later `fs()` and `restart()` returns
`FsError::Failed`, while the supervisor holds its generation where it was, since
no new epoch started. Restarting again is refused on purpose: the hooks would
revoke a second time, and whatever made the disk unreadable is still there.
Recovery belongs to the supervisor, which drops the cell and starts one on a
device that mounts.

**Why it is a `Cell`.** It was not, once: the trait wanted `Send + 'static`, a
`State: Default`, and an infallible `spawn`, and a mount is none of those — it
owns a device that carries a lifetime, and mounting is the operation that fails.
Rather than bend the filesystem to the trait, the trait bent to its first real
implementor: `spawn` returns `Result<Self, Self::Error>`, `restart` puts a
cell back in place instead of rebuilding it from `Default`, the message pair
moved to a separate `Handler` (the filesystem takes an owner and a buffer
registry per request, so a `Message`/`Reply` around `FsOp` would say nothing),
and the thread bounds went with them — `Loopback<'a>` and the kernel's borrowed
`Block<'_>` are the disks that exist, so a `'static` supervisor could supervise
neither. `State = D` is the device the cell owns for its life, `Error = FsError`
is what mounting already returns, and `RestartHooks` plus
`CapabilityTable::revoke_owner` do what they did before, in the supervisor now
so that one place fixes the order.

**The seam is still the ring.** `Fs::serve` drains submissions and posts
completions without knowing who submitted them, and `Session` submits without
knowing who serves. Nothing above the ring reaches into the volume — the shell
has a capability and a buffer, and that is all it has — so the cell wraps a
boundary that was already load-bearing rather than inventing one.

**The client is a cell too.** `molt_shell::Shell` implements `Cell` with a
`Session` as its state: it starts holding no lease, and a restart resets the
session — the lease goes, the pending request is abandoned, and the completions
still in the ring are drained so the next command does not read an answer to a
question the previous epoch asked. Its supervisor's hooks are
[`Disconnect`](../crates/molt-fs/src/restart.rs), which cancels what the shell
submitted and revokes what it held, because a cell that has stopped cannot close
its own handles. That is the second half of the restart story: until now every
revocation in the system was the service taking back what it had minted, and
this is the supervisor of the *holder* giving it up.

**And two cells make a tick worth counting.** `Supervisor::record_heartbeat`
existed with no reader, which is another way of saying nothing in the system had
a liveness policy. It has one now: `watch(tick, deadline, hooks)` restarts a cell
whose last heartbeat is more than `deadline` ticks old, and init calls it after
every line the shell runs. A line the shell finishes is the heartbeat; a line
nothing answers is a missed one; two in a row and the shell is restarted by
something other than the code that started it. The tick is a line rather than a
timer on purpose — nothing preempts a cell here, so between units of work is the
only place a supervisor can honestly look at a clock, and a timer would report a
cell as late while it was still holding the only core.

**The block driver is a ring now.** It was a call for as long as the argument
for one held: a ring whose only client reads a block and immediately needs it is
a queue of depth one with an await around it, and the version worth building is
the one with readahead and concurrent extent fetches. That is the version below.
`Volume` and `Journal` are `async` over `BlockOp`, and `Fs::on` is where the
awaiting stops — an `FsOp` gets an answer, not a future, so the service polls
the request it is serving and gives the block driver its turn between polls. The
executor question the call was avoiding never arrived: `drive` is twenty lines
and the filesystem still has one task, which is enough because the concurrency
that matters is between the block requests of a single operation, not between
operations.

**Naming.** `FsCell` is the one suffix in the crate, and it earns the exception:
`Fs` is the protocol a client talks and `FsCell` is the service that owns one,
and the two are exported side by side, where `Fs` and `Cell` alone would not say
which is which. Everything else keeps the rule from
[the style guide](style.md) — `molt_virtio::Block`, `molt_block::Loopback` — and
nothing is called `VirtioCell`, because a driver behind a ring is still a
driver: `Backing` owns a device and answers submissions, and has no lifecycle
for a supervisor to restart.

## Where the driver ends: `molt-block`

The concern raised against Stage 2.3 was that `molt-virtio` mixes a general
block driver with virtio specifics, and it was correct. `Block::read` was the
only way to read a sector, so a filesystem written against it would have
inherited the virtqueue, and a loopback device or an NVMe driver would have
meant a second read path in the filesystem.

`molt-block` is the split. Reads and durable mutation are separate contracts:

```rust
pub trait Device {
    fn sectors(&self) -> u64;
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError>;
}

pub trait Writable: Device {
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), BlockError>;
    fn flush(&mut self) -> Result<(), BlockError>;
}
```

`Volume` needs only `Device`; `Journal` and `Fs` require `Writable`. A
read-only loopback remains useful and rejects attempted mutation with
`BlockError::ReadOnly`, while a mutable loopback and fault-injection device
implement the durable side. Everything about BARs, virtqueues, DMA arenas, and
interrupt vectors stays below these traits.

Three details in that signature are decisions:

- **A read is all-or-nothing.** It fills `buf` completely or it fails. Short
  reads would push a resume loop into every caller above for a case that only a
  broken device produces. `bounds` is the shared check every implementor gets,
  so `Unaligned` and `Range` mean the same thing on every device.
- **Sectors, not blocks.** 512 bytes is what devices are addressed in;
  translating to the filesystem's 4096 is `Volume`'s job and is one
  multiplication. Making the trait speak 4096 would have baked a filesystem
  constant into the storage layer.
- **`impl<D: Device + ?Sized> Device for &mut D`.** A filesystem takes its
  device by value, because owning it is what lets it hold a mount. The kernel
  smoke has to reset the same virtio device afterwards, so it lends the driver
  instead of giving it away. One blanket impl covers both, and the reset stays
  the driver owner's business.
- **`flush` is the persistence boundary.** `write` promises later reads through
  the same device see bytes; only a successful `flush` promises those bytes
  survive power loss. The filesystem never infers durability from request
  completion.

`Loopback` is what makes the filesystem testable on the host: `molt-fs`'s entire
suite mounts real images out of `Vec<u8>` with no QEMU, no device, and nothing
mocked — the same reader the kernel runs, over the same bytes the kernel would
read. That is the practical payoff of the split, and it arrived the day the
trait did.

**A ring above the traits.** `Device::read` returns when the sectors are there,
which is right for one reader and wrong for everyone else: nothing can be in
flight while it waits, so a reader that already knows it wants the next extent
has no way to say so. `molt_block::ring` is the queue that lets it:

```rust
pub enum BlockOp {
    Read { sector: u64, bytes: usize, buffer: Buffer },
    Write { sector: u64, bytes: usize, buffer: Buffer },
    Flush,
}
```

- **The buffer travels with the operation.** A read hands its buffer over and
  gets it back on the completion, so the queue carries no borrow and no
  lifetime, and there is nothing for the device to alias while the submitter is
  somewhere else. That is the part `&mut [u8]` in a trait cannot do once the
  read outlives the call.
- **Answers come back in whatever order the device finished them.**
  `BlockClient::take` walks past the ones it is not waiting for and parks them;
  a ring `N` deep parks at most `N - 1`, so a caller awaiting the read it needs
  never loses the readahead it does not. The one order that matters is stated
  rather than assumed, and it is the flush below.
- **`Backing` is the bottom.** It owns the driver end and the device, and `run`
  is `drive(future, || driver.pump(queue))`: poll the task, serve what it
  queued, repeat. Awaiting stops there, because something has to move sectors.

`bytes` is on the operation because a checkpoint writes one sector out of a
4 KiB buffer and a data read fills all eight; making the ring speak blocks would
have meant a second path for the superblock.

**A queue below the ring.** The ring let eight reads be outstanding above the
driver, and the driver then ran them one at a time, because a `Disk` answers one
call before it hears the next. That is a device kept at a queue depth of one no
matter how much the filesystem asks for, and it is the difference between a
device working on four requests and a device idle for three of them. `Queue` is
the depth stated where the device is:

```rust
pub trait Queue {
    fn sectors(&self) -> u64;
    fn depth(&self) -> usize;
    fn start(&mut self, id: RequestId, op: BlockOp) -> Result<(), BlockOp>;
    fn reap(&mut self) -> Option<(RequestId, BlockDone)>;
}
```

- **`start` and `reap` are separate because in flight is a state.** A request
  the device has taken and not answered is neither submitted nor complete, and
  the trait that returns an answer from the call it was asked in has nowhere to
  put it. `depth` is how many of those the device holds, and the driver fills to
  it: `pump` drains what finished, feeds what fits, and repeats until neither
  moves.
- **The id travels down, not just the buffer.** A device that answers out of
  order has to say what it is answering, and `RequestId` is what the ring
  already routes by, so a driver with several descriptors outstanding maps them
  to ids instead of to the one command in progress.
- **A flush runs alone.** The journal's crash consistency is an ordering claim —
  these writes are durable before that superblock — and a device free to reorder
  would break it. So the driver holds a flush until everything outstanding has
  answered and starts nothing beside it, which makes the boundary explicit
  instead of a side effect of a depth of one.
- **`Serial<D>` is `Queued<D, 1>`.** Any blocking `Disk` becomes a queue of a
  depth the caller picks: the slots are a const-generic array in the struct, so
  nothing allocates, and `Serial::new(device)` is what every mount that has no
  real queue underneath writes. A driver whose hardware queues for itself
  implements `Queue` and skips the adapter.

`Queued` answers newest first on purpose. A host device that returns things in
the order they were asked would let an ordering bug live in the tree until real
hardware found it; the one that reorders is the one worth testing against, and
`tests/reads.rs` mounts over a device eight deep to assert the fetch counts hold
either way.

**What the depth is worth.** `Loopback` has no flight time to hide, so the same
test file attaches a `Slow` queue that holds every answer for sixteen turns and
counts the turns the driver spends waiting. Streaming 256 KiB through a 4 KiB
window costs **1792 turns at depth one and 847 at depth eight** — 2.1× — and
that is with readahead three blocks ahead, which is what the volume asks for
today. The number is a count rather than a clock, so it is the same on every
machine and it fails when the depth stops reaching the device.

## Slots, readahead, and what the ring cost

A queue is only worth its await if something puts more than one request in it.
`Volume` is that something: up to `SLOTS = 8` block buffers, each `Here` with
the block it holds, `Flight` at the device with a `RequestId`, or `Lent` to a
submission not taken back yet, over a ring `2 * SLOTS` deep so the ring is never
the reason a read waits.

**A sequential read asks ahead.** `Volume::read` submits the next `AHEAD = 3`
blocks of the extent it is on before awaiting the one it needs, so what a
streaming `cat` is about to want is already at the device when it asks. The run
is known by then, so this costs no metadata read — only a guess about where the
caller goes next. Being a guess is what shapes the rest: it stops at the end of
the run, because the block after a hole is not a block, and it declines when
there is no free slot rather than waiting for one.

**A region walk spends every slot.** Mount verifies six regions, and a commit
sums what it wrote; both walk a range in order. `sweep` keeps starting reads on
free slots ahead of the block it is handing over, and releases each block as it
is taken: a walk reads a region through, and nothing behind it is coming back.
That is what keeps the pool from being spent on blocks a mount will never look
at again.

**The pool fills as reads ask for it.** Slots are allocated lazily up to
`SLOTS`, and `free` prefers an empty slot, then a fresh one, then the
second-chance hand. Eight zeroed 4 KiB buffers is roughly 2 µs of allocation a
mount does not need and cannot use — it ends up holding two, while a lookup and
a stream reach eight.

**What it fetched.** Fetches are the part of this that counts the same on every
machine, so they are a test rather than a benchmark: a 256 KiB file read through
a 4 KiB window costs 97 fetches where the single-block window cost 255, and half
the windows fetch nothing at all, because what they wanted landed while the
window before them was being filled. `crates/molt-fs/tests/reads.rs` asserts
both, and both fail on the commit before the ring.

**What it cost, over loopback.** Medians of three interleaved rounds against the
commit before the ring:

| Benchmark | Call | Ring |
| --- | --- | --- |
| `fs_mount` | 11.98 µs | 14.05 µs |
| `fs_open` | 760 ns | 878 ns |
| `fs_read_stream`, 256 KiB through a 4 KiB window | 1.903 ms | 1.990 ms |
| `fs_commit`, per commit | 647 µs | 629 µs |

Reads got slower, which is the honest result and the expected one. Every
benchmark runs over `Loopback`, where a fetch is a `memcpy`: there is no device
time to overlap, so what these measure is the ring's own cost, around 100 ns a
block — a submission, a yield, a pump, and a re-poll of the coroutine where
there used to be a call. `fs_commit` comes out slightly ahead, which is inside
the band [a shared runner produces](testing.md) and is not claimed as a win. The
read numbers turn over the day a fetch costs more than a `memcpy`, which is
exactly what readahead is for and what a real device already does. Nothing here
proves that, and a fake device with a latency loop would not either: `Device` is
synchronous, so a spinning fake overlaps nothing. The number that proves it
comes from virtio, and needs the driver's own queue depth to be worth taking.

## The protocol

```rust
pub enum FsOp {
    Open { dir: Capability<Dir>, name: Name },
    Entry { dir: Capability<Dir>, index: u32 },
    Read { file: Capability<File>, buffer: BufferOperation<Write>, offset: u64 },
    Create { dir: Capability<Dir>, name: Name, kind: Kind },
    Write { file: Capability<File>, buffer: BufferOperation<Read>, offset: u64 },
    Sync,
    Stat(Handle),
    Close(Handle),
}
```

Eight operations, and the shape of each one is the argument.

**Nothing carries data.** A read names a buffer with `Write` authority; a write
names one with `Read` authority. Only the supervisor-owned registry turns
either into memory, so neither side passes a pointer. This is the same
discipline `molt-arch::dma` applies to a device, one layer up, and it keeps
`FsOp` `Copy` and small enough for a ring slot.

**`Capability<Dir>` and `Capability<File>` are different types.** Not a flag on
one handle: distinct rights markers, so `Read { file: ... }` cannot be written
against a directory and `Entry { dir: ... }` cannot be written against a file.
The kind check that a POSIX filesystem does at runtime with `EISDIR` mostly
happens at compile time here, and `FsError::Kind` exists for the case the client
genuinely does not know yet — it opened a name and got back whichever kind was
there.

**Open handles carry `Rights::READ_WRITE`.** `Create` requires a directory
handle, `Write` requires a file handle, and revocation invalidates both rights
by advancing one capability generation. A stale file capability therefore
cannot write after its owner restarts. `Sync` returns the generation that is
durable when it completes; without pending mutations it is a barrier and keeps
the current generation.

**The root handle comes from nowhere, and off the ring.** Every other handle is
opened from a directory somebody already holds; the first cannot be, so there is
no `FsOp` for it. `Fs::root` mints it, and only code holding the mounted `Fs` —
the service itself — can call that. `Fs::publish` mints one root, puts it in the
registry as the `Storage` endpoint, and calls `Fs::seal`, after which the grant
is gone for the mount's life and a later caller gets `FsError::Sealed`. That
single asymmetry is where a namespace would otherwise be: authority to reach the
tree enters the system once, from the one place that already has all of it,
rather than being mintable by anything that can submit an operation. What
changed when the registry arrived is only who receives it — the root goes to a
publication clients lease rather than into a client's hand — which is what lets
the mount underneath a holder be replaced.

**A listing carries `Stat`.** `FsDone::Entry` returns the name *and* the kind,
size, and entry count, because the volume already read the object record to
answer at all. `ls` printing a size costs one round trip per directory, not one
per name. The separate `Stat` operation stays for the case where a client holds
a handle and never listed its parent.

**Ownership is not authority.** `Fs::apply` takes a `CellId`, but it is the
owner recorded for *new* handles, not a check against the ones an operation
names. Holding the capability is the authority to use it — that is what a
capability is — and the owner exists so that revoking a restarted cell takes its
handles with it.

**Completion is a `Result`.** The ring carries `Result<FsDone, FsError>`, so a
failed operation is an ordinary completion with the request's own ID rather than
an out-of-band signal. `FsError` names what failed precisely enough for the
shell to print it — `Missing`, `Kind`, `Name`, `Range`, `Checksum`, `Corrupt` —
and wraps the layers below it rather than flattening them: a device failure
stays `Device(BlockError)`, a stale handle stays `Handle(CapabilityError)`.

**Completion backpressure loses nothing.** Submission and completion queues are
independent even though they have the same capacity. If a reply cannot be
published, `Fs` retains that completion and stops draining submissions until the
client makes room, so an operation is applied once and its answer is not
discarded.

## The shell

`molt-shell` is a client and nothing more, and that is its job: it exists to
prove the protocol is usable by something that was not written to compensate for
it. If `cat` had needed to reach into the volume, the protocol would have been
wrong.

`Session` holds the client end of the ring and the scratch buffer reads land in,
and it is where the buffer discipline shows up concretely. The registry lives in
a `RefCell` shared with the driver; the filesystem borrows it inside `serve`,
the shell borrows it inside `Session::taken` — and neither holds a borrow across
an await, which is what makes the runtime check never fire. The two capabilities
`Session::new` attenuates from one registration are the other half: the
filesystem gets the right to fill the buffer, the shell gets the right to look
at what landed, and neither can do the other's half.

`request` submits and awaits. Nothing wakes the task when the answer arrives —
the driver runs on the same loop and posts completions without a waker — so a
poll that finds the queue empty wakes itself and returns `Pending`. That is
honest about there being no interrupt rather than pretending one is coming, and
it is the shape that survives the driver becoming interrupt-driven: the waker
call moves, the await does not.

`drive` is the loop underneath: poll the future, run the driver, repeat. Twenty
lines, one task, a noop waker, and no claim to be an executor. `molt-core` has
one of those; a shell in a boot log does not need it, and using it here would
have hidden how little machinery the ring protocol actually requires. It lives
in `molt_core::task` rather than in the shell, because the block ring wants the
same loop at the other end of the system — `Backing::run` is `drive` with a
`pump` in it — and two copies of twenty lines is where the second one starts
drifting.

`ls`, `cat`, and `help` are what the roadmap asked for. `cat` reads through a
window deliberately smaller than the files on the disk, so the loop, the offset
arithmetic, and the short read at the end of a file are all exercised every time
it runs rather than only on a large file somebody remembers to test.

Input is a script. No platform reads its serial port back yet, so `Shell::run`
takes a line from wherever the caller found one; an interactive front-end is a
line editor away and needs a serial `read` before it is worth writing.

## What this stage does not do

- **No rename, unlink, or compaction.** Create, sparse write, replay, and sync
  are complete; reclaiming log space and changing namespace links arrive with
  the B-tree/free-space stage.
- **No snapshots, no reflinks, no compression, no encryption.** Stage 4, and
  each one needs the writer first.
- **No data cache.** Metadata nodes are cached; `Volume` keeps eight block
  buffers and nothing beyond them. That is enough for a directory search to
  re-read a block only when the binary search moves off every slot, and for the
  sums block covering a file to stay resident while the file streams past it —
  and it is a window, not a cache: it holds what one mount is doing now, and
  forgets it when the slot is worth more. A cache that outlives the operation is
  the page cache below, which needs the writeback policy this stage does not
  have.
- **No scrub.** The sums region exists and is checked per block on read; walking
  it deliberately is a Stage 4 item, and the region layout is what makes it
  cheap when it comes.
- **The depth stops at virtio.** `Backing` hands the device as many requests as
  its `Queue` takes, so nothing above the driver limits what is in flight any
  more — but `molt_virtio::Block` is still one command at a time behind a
  `Serial`, which is a queue of depth one. The depth becomes real when the
  driver keeps several descriptors outstanding and routes a used entry by
  request id rather than by the one command in progress.
- **One operation at a time above the ring.** `Fs::on` drives an `FsOp` to its
  answer before taking the next, so the concurrency the block ring buys is
  between the reads of a single operation. Overlapping two clients' requests
  needs an executor in the cell and a `Journal` that can have two borrows out,
  which is a scheduling change, not an I/O one.
- **No SMP.** One core, one mount, no locks: the metadata cache is `Rc`, `Fs`
  is `&mut`, and neither is `Send`. That is a bound the type system states
  rather than a bug waiting — a second core cannot reach this filesystem by
  accident. Stage 4 gives the block layer its own concurrency story, and the
  shape that survives it is a filesystem *cell* other cores talk to by ring,
  which is why the service exists now and the locks do not.
- **No `Zbc` crc32c.** x86_64 folds through the `crc32` instruction and
  everything else through the table, which is 15× the bit-at-a-time loop it
  replaced and enough that riscv64 is not the machine the checksum is measured
  on. The Zbc path is the same one-file change the x86_64 one was.

## How it is tested

Everything with arithmetic in it is a host test over a real image, because
`Loopback` made that possible: format round-trips through builder and reader, a
torn superblock is refused, a foreign block is refused, a future version is
refused by version rather than checksum, a region past volume end is refused,
and a damaged region fails at mount rather than first use. Reads cover block
boundaries, sparse holes, file end, and writes that overlay immutable data and
extend it through a hole.

A checksum-valid but impossible extent is still refused: physical block
arithmetic is checked before a data read, so a malformed address cannot wrap
into another region or panic the reader.

Ring tests are about who gets which answer: a read lands through the ring, an
answer parked while a later one is awaited comes back to whoever asked for it,
a full ring refuses a submission rather than dropping one, and a write is
visible on the disk only after its flush. Above them `tests/reads.rs` counts
fetches per window, which is the counted cost of readahead and holds on every
machine.

Service tests cover protocol rather than format: create/write/sync survives a
remount, dynamic `Stat` sees new size, a read lands only in its registered
buffer, and a revoked owner cannot write through a stale file handle. A full
handle table refuses rather than overwrites, and a full completion queue
preserves the next result until the client makes room.

Cell tests cover the lifecycle: a restart keeps what was synced, drops what was
not, and leaves every handle stale, while a supervised cell over a borrowed disk
runs its hooks in the documented order and counts the generations — the same
test that would stop compiling if `Cell` asked for `'static` again. A disk that
stops answering reads mid-restart proves the failed state: the cell reports the
device error, refuses every later call, and does not run its hooks again even
once the disk is back. Two tests hold the stack budget
at 16 KiB for mount and for a create/sync cycle.

The shell tests run scripts against a mounted image and compare what was
printed, which is the only test that can catch a protocol that is technically
complete and unusable: `cat` across several reads, `cat` on a directory, `ls` on
a file, a name that does not exist reported rather than returned, and a command
that does not exist naming itself. Four more run the shell as a cell rather than
a script: a shell with nothing published says `no storage` instead of hanging, a
withdrawn mount is reported and then re-acquired once it is republished, an
answer to a command that was abandoned is skipped rather than printed against
the next one, and a restarted shell loses the directory it had open — its hooks
revoke exactly one handle. A fifth is the watchdog policy itself, driven the way
init drives it: two lines nobody serves, a `watch` call after each, one restart
on the second, and a working `ls` afterwards to prove the cell came back.

`cargo xtask mkfs <tree> <image>` writes a directory tree out as a mountable
image, and the smoke disk is one of those rather than a signed pattern — the
`disk/` tree in the repository, laid out at smoke time. The block driver reads
sector zero, the filesystem mounts, creates `runtime.txt`, writes and syncs it
through virtio, reads it back, publishes its root, and only then does the shell
run — against a lease it acquired itself. The x86_64 smoke requires
`MOLT_FS_WRITE_OK:` in addition to mount and shell markers. Mid-script the
service restarts under the shell, which requires `MOLT_REGISTRY_OK:` for the
older leases going stale; two starved lines later the shell is restarted by its
own supervisor, which requires `MOLT_WATCHDOG_OK:`. The `cat` that prints the
original host file is the line after both, so the markers that were already
required — `molt> cat hello.txt` and its contents — now assert that a client
survives losing its service and then losing itself. Init then restarts the
service once more and requires `MOLT_FS_RESTART_OK:` for the synced file still
being there. An xtask test performs the same write, drops the mount, remounts,
and checks the durable bytes on the host.

## Debts closed before the write path

Stage 3 is the writable filesystem, and three things were cheaper to settle
while the format still has no long-lived images than after. None is a write
feature; each is a decision the write path would otherwise inherit wrong.

**`MAX_NAME` is 255, and inline.** The read-only stage shipped it at 64, which
was enough for a boot image and wrong for a filesystem: 255 is the limit every
mainstream filesystem settled on, and the largest a one-byte inline length can
hold. Fixing it now costs nothing on disk — names live out of line under a
`u16` length, so a wider reader bound reinterprets no stored byte and does not
move the version. What it does widen is the inline [`Name`](../crates/molt-fs/src/name.rs)
a ring carries, to 256 bytes, and with it every ring slot: `FsOp` and `FsDone`
reach 272 bytes each. The alternative considered was a `Cow`-shaped name —
inline for short leaves, a registered-buffer reference for long ones — and it
was rejected. It puts a resolver on the hottest path, `Open` and `Entry`, to
save bytes on a message that is already `Copy` and already fits a stack ring
with room to spare; the ceremony of registering a buffer for a path is exactly
what the inline name exists to avoid, and 256 bytes is a bound a kernel stack
does not feel. The version stays 1: the encoding did not change, only the
reader's tolerance for it.

**A ring slot's size is asserted, not assumed.** `op.rs` carries
`const _: () = assert!(size_of::<FsOp>() <= 512)` and the same for `FsDone`, so
raising `MAX_NAME` again — or adding a variant that carries something large —
fails the build rather than quietly growing a message every submission and
completion copies by value. 512 is where a message stops being a thing to pass
on the stack without thinking about it; the current 272 leaves the headroom on
purpose. The `large_enum_variant` lint fires alongside, because only the
name-carrying variant is big, and it is allowed with the same reasoning the
assert records: the imbalance is the inline name, and boxing it needs an
allocator this layer refuses.

**Root enters once and the door shuts.** The read-only stage let any client
submit `FsOp::Root` and receive a root handle, which made the one piece of
ambient authority in the design mintable by anyone on the ring. It is now off
the ring entirely: `Fs::root` is the only grant, only init holds the `Fs` to
call it, and `Fs::seal` makes it one-shot for the mount's life. A restart is a
new life: every handle from the old one is revoked, so the bootstrap opens again
for a service that has no holders left to protect. The protocol section above is
the full argument; the debt was that the asymmetry existed in prose but not in
the types.

## Version and growth path

Writable COW layout is version 3. Adding the tree arena and root changes bytes
and geometry an older reader interprets. There is no published standard yet,
but that is a reason to keep migration policy small, not to label incompatible
layouts with the same version. Version 1 and 2 images are rejected rather than
guessed at; `xtask mkfs` rebuilds development images as version 3.

- **Stage 4.4, asynchronous I/O.** Done: `Volume` and `Journal` are `async` over
  a `BlockOp` ring, with readahead and region sweeps above it.
- **Stage 4, scale.** File payloads compact from the journal into extent keys;
  reference counts and bucket generations generalize the bounded tree arena;
  sums become a scrub work list; the block layer gains a data cache and a driver
  that keeps more than one request at the device, both behind the same traits.
- **Stage 5, storage for cells.** A signed cell image is a file with a signature
  region, and the loader is a client of this protocol — which is the argument
  for the protocol being pleasant to write against, since a loader is the next
  thing that has to.
