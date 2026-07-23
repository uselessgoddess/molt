# MoltROFS

Status: Stage 2.4 decision record, July 2026.

Why the first filesystem is read-only, what its five regions are for, which
ideas were taken from bcachefs, Redox, and btrfs and which were left, whether
schemes belong under a cytokernel, and whether a driver is a cell. Written as
the record for `molt-block`, `molt-fs`, and `molt-shell`.

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
3. **What survives a crash.** A read-only volume cannot be torn by its own
   writes, but it *is* written — by `xtask mkfs` today and by Stage 3's
   checkpoint later — and the recovery rule has to exist in version 1 or it
   never will.
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

The cost of the choice is real and bounded: `molt-fs` carries no allocator, no
journal, no free-space map, and no write path. What it does carry — and this is
the part chosen deliberately — is every structure the writable successor needs
to exist on disk from version 1: a generation-stamped superblock kept in two
copies, a checksum over every metadata region, a crc32c per data block, and
extents rather than block pointers. A version 2 that adds a checkpoint tree
adds regions; it does not reinterpret the ones already there.

## Taking from bcachefs rather than btrfs

The brief was btrfs's ideas without btrfs's legacy, leaning bcachefs. The three
things worth taking are the same three in both, and they arrive here in the
cheapest form that is still the real thing.

**Checksums that cover data, not just metadata.** Every data block carries a
crc32c in a region of its own, and every metadata region carries one in the
superblock. This is bcachefs's position — checksums are not optional and not a
mount flag — and it is why [`Volume::mount`](../crates/molt-fs/src/volume.rs)
verifies all five regions before the first lookup rather than discovering
corruption at whatever block a directory search happens to land on. A volume
that mounts is a volume whose metadata is intact, which is a much stronger
statement than "the superblock parsed".

The sums live in their own region rather than beside the blocks they cover.
That costs a second block read per data block, which for a boot-time shell is
nothing, and it buys a scrub that walks one contiguous region instead of
seeking across the volume — the Stage 4 item this leaves room for.

**A generation in the superblock, and a checkpoint that swings it.** Below.

**Extents, not block pointers.** A file is a run of `(logical, blocks, block)`
records, sorted by logical block and binary-searched. Contiguous data costs one
record however long it is, a logical block no extent covers is a hole that reads
as zeros, and `xtask mkfs` drops every all-zero block on the floor — so a sparse
file costs its content, not its length. Extents are also the only structure here
that a writable version would have kept anyway; block pointers would have been
thrown away.

What was deliberately *not* taken:

- **A B-tree, yet.** bcachefs is a B-tree of everything and btrfs is a forest of
  them, and both are right for a filesystem that mutates. A read-only image is
  written once, sorted, and never inserted into, so the same asymptotics come
  from a sorted array with a binary search over it — ten block reads for a
  thousand-name directory — in about a fiftieth of the code. The B-tree is a
  Stage 4 item because Stage 4 is where insertion exists. The format does not
  fight it: entries and extents are already sorted by the key a tree would use,
  so a tree is a new region and a new object field, not a new format.
- **Reflinks, snapshots, subvolumes.** All three need refcounts, which need a
  writer. The room they will take is a superblock field and a region, both of
  which the layout has space for.
- **Inodes as a namespace.** There is no `stat` on a number, no hard links, and
  no `.`/`..`. An object is reached by having opened it; see below.
- **btrfs's on-disk anything.** The item/key/leaf machinery, the chunk tree, and
  the backref format are solutions to problems Molt does not have yet, and each
  one is a compatibility obligation from the moment an image exists.

## The format

Five regions, one superblock, all little-endian, everything block-addressed.
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
```

Blocks are 4096 bytes. Every record size divides the block size, so no record
straddles a boundary and a reader needs exactly one block of buffer to reach any
of them — which is why an object is 32 bytes rather than the 24 its fields use.
That constant is the reason `Volume` can be `no_std`, allocation-free, and
usable from a kernel that has no heap: mounting costs one `[u8; 4096]` the
caller supplies.

**The superblock** carries a magic, a version, the block size, a generation, the
volume length, the root object id, where data starts and how long it is, and the
five region descriptors — each an offset, a length, and a crc32c over the
region's contents. Its own checksum is checked before any field is read, so a
torn write is rejected at the checksum rather than by whatever the region
offsets would otherwise have pointed at. `Super::check` then refuses a
structurally impossible volume: a region starting inside the superblock copies,
a region running past the end, a sums region whose length disagrees with the
data block count.

**An object** is a kind, a start index, a count, and a size. For a directory the
range is into the entries region and the size is zero; for a file the range is
into the extents region and the size is the file's length in bytes. One record
serves both because the difference between them is one byte, and a filesystem
that needs two record types before it has a write path has already spent its
simplicity budget.

**An entry** is a `(name_at, name_len, object)` triple pointing into the name
arena. Names are out of line so an entry stays 16 bytes and a directory search
reads one block per probe regardless of name length; `MAX_NAME` is 64 bytes,
which bounds the copy a lookup makes onto the stack.

## Crash consistency

Nothing here writes to a mounted volume, so the property to preserve is narrower
than Stage 3's and worth stating exactly: **a volume is always mountable at some
checkpoint, whatever moment power is lost.**

The mechanism is two superblock copies and a generation. A checkpoint writes
every region and every data block first, then overwrites the *older* superblock
copy with the new one, and that overwrite is the instant the new state becomes
the volume's state. `Volume::mount` reads both copies, discards any that fails
its magic, checksum, version, or structure check, and takes the newest of what
is left. So:

- Power lost while regions are being written: neither superblock mentions them,
  and the volume mounts at the previous generation.
- Power lost mid-superblock: that copy fails its checksum and is discarded, and
  the other copy — the previous checkpoint — is intact by construction, because
  the copy being overwritten is always the older one.
- Power lost after the superblock lands: the new generation is the newest that
  verifies, which is the definition of the checkpoint having happened.

There is no fsck, and that is a design position rather than an omission: the
things fsck repairs are the things a checkpoint discipline prevents. There is
also no `fsync`, no barrier, and no flush issued anywhere, because nothing in
this stage writes to a device at all — `xtask mkfs` builds an image in memory
and hands it to the host's filesystem. The moment Stage 3 writes to a virtio
device, the checkpoint above needs `VIRTIO_F_FLUSH` between "regions written"
and "superblock written", and that ordering is exactly the thing
[`docs/virtio.md`](virtio.md) records as deliberately absent. The rule survives
the transition; only the flush is new.

What this does not defend against is a device that reorders across the whole
write. That is what the flush is for, and it does not exist yet because the
writer does not either.

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
single `Name` — a leaf, checked to be non-empty, at most 64 bytes, and free of
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
readahead, concurrent extent fetches, and a cache behind it, and all three want
the writable filesystem's structure. Stage 3 gets `BlockOp`; Stage 2.4 gets the
trait below, which is what makes Stage 3's version a substitution rather than a
rewrite.

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

`molt-block` is the split. It is one trait and one implementation:

```rust
pub trait Device {
    fn sectors(&self) -> u64;
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError>;
}
```

That is the entire contract a filesystem needs, and everything about how a
device is reached — BARs, virtqueues, DMA arenas, interrupt vectors — stays
below it. `molt_virtio::Block` implements it over a virtqueue,
`molt_block::Loopback` implements it over bytes already in memory, and a future
NVMe driver implements it over whatever it likes.

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

`Loopback` is what makes the filesystem testable on the host: `molt-fs`'s entire
suite mounts real images out of `Vec<u8>` with no QEMU, no device, and nothing
mocked — the same reader the kernel runs, over the same bytes the kernel would
read. That is the practical payoff of the split, and it arrived the day the
trait did.

## The protocol

```rust
pub enum FsOp {
    Root,
    Open { dir: Capability<Dir>, name: Name },
    Entry { dir: Capability<Dir>, index: u32 },
    Read { file: Capability<File>, buffer: BufferOperation<Write>, offset: u64 },
    Stat(Handle),
    Close(Handle),
}
```

Six operations, and the shape of each one is the argument.

**Nothing carries data.** A read names a registered buffer, and only the
registry — which the supervisor owns — can turn that name into memory. The
driver writes into the client's buffer and neither side ever holds the other's
pointer. This is the same discipline `molt-arch::dma` applies to a device, one
layer up, and it is why `FsOp` is `Copy` and small enough to sit in a ring slot.

**`Capability<Dir>` and `Capability<File>` are different types.** Not a flag on
one handle: distinct rights markers, so `Read { file: ... }` cannot be written
against a directory and `Entry { dir: ... }` cannot be written against a file.
The kind check that a POSIX filesystem does at runtime with `EISDIR` mostly
happens at compile time here, and `FsError::Kind` exists for the case the client
genuinely does not know yet — it opened a name and got back whichever kind was
there.

**`Root` is the one handle that comes from nowhere.** Every other handle is
opened from a directory somebody already holds. That single asymmetry is where a
namespace would otherwise be, and it is why the shell's first line of work is
`FsOp::Root` and everything after it is a hop.

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

- **No writes.** No allocator, no free-space map, no journal, no `fsync`, no
  rename. Stage 3.
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
- **crc32c is a software table.** No `SSE4.2` or `Zbc` path. It is 75 lines and
  the disk it runs against is a boot-time image; the moment it shows up in a
  profile, the intrinsic is a one-file change.

## How it is tested

Everything with arithmetic in it is a host test over a real image, because
`Loopback` made that possible: the format round-trips through the builder and
the reader, a torn superblock is refused, a foreign block is refused, a future
version is refused by version rather than by checksum, a region past the end of
the volume is refused, and a damaged region fails at mount rather than at first
use. Reads cover the boundary cases that a hand-written offset loop gets wrong —
across a block boundary, from a hole in a sparse file, at the end of a file, and
past it.

The service tests cover the protocol rather than the format: an open walks from
the root handle, a read lands in a registered buffer and nowhere else, a closed
handle goes stale, a revoked owner loses every handle at once, and a table with
no free slot refuses rather than overwrites.

The shell tests run scripts against a mounted image and compare what was
printed, which is the only test that can catch a protocol that is technically
complete and unusable: `cat` across several reads, `cat` on a directory, `ls` on
a file, a name that does not exist reported rather than returned, and a command
that does not exist naming itself.

`cargo xtask mkfs <tree> <image>` writes a directory tree out as a mountable
image, and the smoke disk is one of those rather than a signed pattern — the
`disk/` tree in the repository, laid out at smoke time. That is what makes one
artifact prove the whole path: the block driver reads sector zero and finds
`MOLTROFS`, the filesystem mounts the same bytes, and the shell's `cat
hello.txt` prints what the host file contains. The x86_64 boot smoke requires
`MOLT_FS_OK:`, the shell's own `molt> cat hello.txt` and `hello, molt` lines,
and `MOLT_SHELL_OK:` on the serial line. An xtask test lays out the same tree
and mounts it back on the host, so the image is checked even where QEMU is not
installed.

## Growth path

The format's version is 1 and the reader refuses anything else, which is the
right starting posture: a version that is checked from the first image is a
version that can be relied on later.

- **Stage 3, writable.** Regions gain a free-space map and an object bitmap; the
  superblock gains a pointer to a log. The checkpoint rule stays exactly as it
  is, with a device flush between the regions and the superblock. `FsOp` gains
  `Write`, `Create`, `Rename`, and `Sync`, and `Capability<File>` gains
  `Rights::WRITE` — which the type already models and this stage simply never
  hands out.
- **Stage 3, cells.** `Fs` becomes a supervised cell when there is more than one
  client and a restart means a remount. `revoke_owner` is the piece that has to
  work then, and it works now.
- **Stage 4, scale.** Objects and extents become B-tree leaves keyed the way
  they are already sorted; the sums region becomes a scrub's work list; the
  block layer gains its ring, a cache, and readahead behind the same `Device`
  trait.
- **Stage 5, storage for cells.** A signed cell image is a file with a signature
  region, and the loader is a client of this protocol — which is the argument
  for the protocol being pleasant to write against, since a loader is the next
  thing that has to.
