# MoltFS

Status: Stage 3 COW B-tree filesystem, July 2026.

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
That costs a second block read per data block, which for a boot-time shell is
nothing, and it buys a scrub that walks one contiguous region instead of
seeking across the volume — the Stage 4 item this leaves room for.

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
That constant is the reason `Volume` can be `no_std`, allocation-free, and
usable from a kernel that has no heap: mounting costs one `[u8; 4096]` the
caller supplies.

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
on top rather than teaching the tree about files. The path and split buffers
are fixed-size, so the kernel still needs no RAM allocator.

The tree arena has a bounded tracing allocator. Starting a transaction marks
nodes reachable from the active and previous roots; every other arena block is
reclaimable. Replaced paths created in the same transaction are released
immediately. This keeps both crash fallbacks intact while allowing old
generations to be reused without fsck. `build_with_capacity` selects tree and
log capacity, and `FsError::Full` reports either finite bound explicitly.

## Metadata cache

`MetadataTree` owns four parsed-node slots, about 16 KiB, and uses a bounded
second-chance policy inspired by
[SIEVE](https://www.usenix.org/conference/nsdi24/presentation/zhang-yazhuo).
A hit sets one visited bit and does not move the node. The eviction hand clears
visited candidates and replaces the first unvisited one. This is useful here
because it has constant metadata, no linked allocation, and makes repeated
root and directory probes device-read free. `Journal::tree_stats` exposes hit,
miss, and eviction counters for tests and diagnostics.

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

There is deliberately one outstanding block request at a time. That makes
ordering observable and deterministic; barriers separate durability epochs,
while the queue cannot reorder requests within one. `molt-block::Fault` models
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

## Cells: not yet, and the reason is measurable

The sketch in the issue had `AppCell → FsCell → VirtioCell`, three cells and two
rings. What shipped is a shell and an `Fs` on one ring, with the block driver
called directly. That is a smaller thing than the sketch, and the difference is
worth being precise about, because "make everything a cell" is the kind of
decision that is very hard to walk back.

**What a cell buys.** `Supervisor` gives restart with state reset, and
`CapabilityTable::revoke_owner` makes that restart *clean*: every handle the
cell held goes stale, so nothing survives a restart holding authority it was
granted in a previous life. `Fs::revoke` is exactly this, and it is already
tested — a cell that dies loses its handles, and a stale handle is
`CapabilityError::Stale` rather than a read of whatever now occupies that slot.
That property is the point of a cell and it exists today.

**What a cell costs, here.** Wrapping `Fs` in `Cell` means committing to
`Message`/`Reply` for something that already has a richer protocol, and today
would mean one message type that wraps `FsOp` and one reply that wraps
`FsDone` — a layer whose entire content is the word "wraps". The supervisor also
restarts a cell by rebuilding its state from `Default`, and a mounted volume's
state is a borrowed block buffer and a device; the honest restart story for a
filesystem is remount, which is a Stage 3 conversation with a write path in it.

**So the boundary is the ring, and the ring is already there.** `Fs::serve`
drains submissions and posts completions without knowing who submitted them, and
`Session` submits without knowing who serves. Putting either end in a cell later
is a change to the outside of those two types. Nothing above the ring reaches
into the volume — the shell has a capability and a buffer, and that is all it
has — so the seam that a cell boundary would need is load-bearing today, which
is the only way to know it is real.

**The block driver stays a call, deliberately.** A `BlockOp` ring under the
filesystem is the sketch's second ring, and it is the one place the layering
argument turns into a cost: a filesystem that awaits its device needs its own
executor to await *on*, and this stage has one task and a `drive` loop. Worse,
it would be a ring whose only client is synchronous — `Volume` reads one block
and immediately needs it — so the ring would be a queue of depth one with an
await around it. The interesting version of that ring is the one with
readahead, concurrent extent fetches, and a cache behind it. That later scale
stage gets `BlockOp`; direct traits keep this stage synchronous and make that
change a substitution rather than a rewrite.

**Naming.** No type here is called `FsCell` or `VirtioCell`. The module supplies
the context, as [the style guide](style.md) says: `molt_fs::Fs`,
`molt_virtio::Block`, `molt_block::Loopback`. If one of them becomes a cell, it
becomes one by implementing `Cell`, and the trait in the `impl` line says so
better than a suffix on every use site.

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
init — can call that. Init hands each first holder its root and then calls
`Fs::seal`, after which the grant is gone for the mount's life and a later
caller gets `FsError::Sealed`. That single asymmetry is where a namespace would
otherwise be: authority to reach the tree enters the system once, from the one
place that already has all of it, rather than being mintable by anything that
can submit an operation.

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
have hidden how little machinery the ring protocol actually requires.

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
- **No B-tree, no snapshots, no reflinks, no compression, no encryption.** Stage
  4, and each one needs the writer first.
- **No cache.** `Volume` keeps the last block it read and nothing else. A
  directory search re-reads a block only when the binary search moves off it,
  and a data block costs a second read for the sum that covers it. A real cache
  is an allocator and an eviction policy, and it wants the SMP story from Stage
  4 to be safe rather than merely correct on one core.
- **No scrub.** The sums region exists and is checked per block on read; walking
  it deliberately is a Stage 4 item, and the region layout is what makes it
  cheap when it comes.
- **No `BlockOp` ring.** Above.
- **crc32c uses a software fallback.** No `SSE4.2` or `Zbc` path. It is 75 lines and
  the disk it runs against is a boot-time image; the moment it shows up in a
  profile, the intrinsic is a one-file change.

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

Service tests cover protocol rather than format: create/write/sync survives a
remount, dynamic `Stat` sees new size, a read lands only in its registered
buffer, and a revoked owner cannot write through a stale file handle. A full
handle table refuses rather than overwrites, and a full completion queue
preserves the next result until the client makes room.

The shell tests run scripts against a mounted image and compare what was
printed, which is the only test that can catch a protocol that is technically
complete and unusable: `cat` across several reads, `cat` on a directory, `ls` on
a file, a name that does not exist reported rather than returned, and a command
that does not exist naming itself.

`cargo xtask mkfs <tree> <image>` writes a directory tree out as a mountable
image, and the smoke disk is one of those rather than a signed pattern — the
`disk/` tree in the repository, laid out at smoke time. The block driver reads
sector zero, the filesystem mounts, creates `runtime.txt`, writes and syncs it
through virtio, reads it back, then the shell prints the original host file.
The x86_64 smoke requires `MOLT_FS_WRITE_OK:` in addition to mount and shell
markers. An xtask test performs the same write, drops the mount, remounts, and
checks the durable bytes on the host.

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
call it, and `Fs::seal` makes it one-shot for the mount's life. The protocol
section above is the full argument; the debt was that the asymmetry existed in
prose but not in the types.

## Version and growth path

Writable COW layout is version 3. Adding the tree arena and root changes bytes
and geometry an older reader interprets. There is no published standard yet,
but that is a reason to keep migration policy small, not to label incompatible
layouts with the same version. Version 1 and 2 images are rejected rather than
guessed at; `xtask mkfs` rebuilds development images as version 3.

- **Stage 3, cells.** `Fs` becomes a supervised cell when there is more than one
  client and restart means remount. `revoke_owner` is the piece that has to
  work then, and it already covers write authority.
- **Stage 4, scale.** File payloads compact from the journal into extent keys;
  reference counts and bucket generations generalize the bounded tree arena;
  sums become a scrub work list; block layer gains its ring, data cache, and
  readahead behind the same traits.
- **Stage 5, storage for cells.** A signed cell image is a file with a signature
  region, and the loader is a client of this protocol — which is the argument
  for the protocol being pleasant to write against, since a loader is the next
  thing that has to.
