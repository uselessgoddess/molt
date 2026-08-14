//! What a submission is on the wire, and what it parses into.
//!
//! A slot is a fixed number of little-endian words with no invalid bit pattern;
//! a [`Call`] is what one of them means once it has been checked. The kernel
//! never holds a `Call` it did not build out of words it had already copied, so
//! no field is left for the other end to change after the check.

use crate::nospec::Mask;

pub const SLOT_BYTES: usize = 64;
pub const SLOT_WORDS: usize = SLOT_BYTES / 8;

/// The tier-1 aperture every offset in a submission is measured inside.
///
/// A [`Region`] is two `u32`s because the aperture is 4 GiB, so the check that
/// one lies inside it is a single unsigned compare that cannot wrap once it is
/// done in `u64`.
pub const APERTURE: u64 = 1 << 32;

/// A capability named by a submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct Handle(u64);

impl Handle {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A buffer, as an offset and a length from the domain's base.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Region {
    offset: u32,
    len: u32,
}

impl Region {
    pub const fn new(offset: u32, len: u32) -> Self {
        Self { offset, len }
    }

    pub const fn offset(self) -> u32 {
        self.offset
    }

    pub const fn len(self) -> u32 {
        self.len
    }

    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Whether this region lies inside `bytes`.
    ///
    /// The only thing here worth branching on. What the branch carries has to
    /// come from [`within`](Region::within), which keeps the refusal true while
    /// the branch is still a guess.
    #[inline(always)]
    pub fn fits(self, bytes: u64) -> bool {
        self.inside(bytes).passed()
    }

    /// This region masked against `bytes`: itself when it fits, and the empty
    /// region at offset zero when it does not.
    ///
    /// The value to hand to the load, whether or not [`fits`](Region::fits) has
    /// been asked yet. [`nospec`](crate::nospec) says why.
    #[inline(always)]
    pub fn within(self, bytes: u64) -> Self {
        let inside = self.inside(bytes);
        Self {
            offset: inside.apply(self.offset as u64) as u32,
            len: inside.apply(self.len as u64) as u32,
        }
    }

    /// The one bounds check both of those come down to. The sum cannot wrap:
    /// two `u32`s widened into a `u64` before they are added.
    #[inline(always)]
    fn inside(self, bytes: u64) -> Mask {
        Mask::of(self.offset as u64 + self.len as u64 <= bytes)
    }

    /// Reads a region out of one wire word, rejecting one outside the aperture.
    ///
    /// The mask is applied above the branch, so the region an `Ok` carries is
    /// masked on the path the processor guessed as well as the one it takes.
    fn decode(word: u64) -> Result<Self, Reject> {
        let region = Self { offset: word as u32, len: (word >> 32) as u32 };
        let masked = region.within(APERTURE);
        if region.fits(APERTURE) { Ok(masked) } else { Err(Reject::Region) }
    }

    const fn encode(self) -> u64 {
        self.offset as u64 | (self.len as u64) << 32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Op {
    Read { cap: Handle, offset: u64, buf: Region },
    Write { cap: Handle, offset: u64, buf: Region },
    Flush { cap: Handle },
    Open { dir: Handle, name: Region },
    Close { cap: Handle },
    Send { cap: Handle, buf: Region },
    Recv { cap: Handle, buf: Region },
    Timer { ticks: u64 },
    Message { channel: Handle, buf: Region },
    Grant { channel: Handle, cap: Handle, rights: u32 },
}

impl Op {
    /// The tag this operation is written as.
    pub const fn tag(&self) -> u32 {
        match self {
            Self::Read { .. } => 1,
            Self::Write { .. } => 2,
            Self::Flush { .. } => 3,
            Self::Open { .. } => 4,
            Self::Close { .. } => 5,
            Self::Send { .. } => 6,
            Self::Recv { .. } => 7,
            Self::Timer { .. } => 8,
            Self::Message { .. } => 9,
            Self::Grant { .. } => 10,
        }
    }

    /// The buffer this operation names, for the caller's own extent check.
    pub const fn region(&self) -> Option<Region> {
        match *self {
            Self::Read { buf, .. } | Self::Write { buf, .. } => Some(buf),
            Self::Send { buf, .. } | Self::Recv { buf, .. } => Some(buf),
            Self::Open { name: buf, .. } | Self::Message { buf, .. } => Some(buf),
            Self::Flush { .. } | Self::Close { .. } => None,
            Self::Timer { .. } | Self::Grant { .. } => None,
        }
    }

    const fn words(&self) -> [u64; 3] {
        match *self {
            Self::Read { cap, offset, buf } | Self::Write { cap, offset, buf } => {
                [cap.get(), offset, buf.encode()]
            }
            Self::Flush { cap } | Self::Close { cap } => [cap.get(), 0, 0],
            Self::Open { dir: cap, name: buf }
            | Self::Send { cap, buf }
            | Self::Recv { cap, buf }
            | Self::Message { channel: cap, buf } => [cap.get(), buf.encode(), 0],
            Self::Timer { ticks } => [ticks, 0, 0],
            Self::Grant { channel, cap, rights } => [channel.get(), cap.get(), rights as u64],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Reject {
    /// Unknown or zero tag.
    Tag = 1,
    /// Reserved/future fields are not zero.
    Reserved = 2,
    /// Buffer outside 4 GiB aperture.
    Region = 3,
}

fn zeroed<T, const N: usize>(unused: [u64; N], guarded: T) -> Result<T, Reject> {
    if unused == [0; N] { Ok(guarded) } else { Err(Reject::Reserved) }
}

fn payload(buf: u64, unused: u64) -> Result<Region, Reject> {
    if unused != 0 {
        return Err(Reject::Reserved);
    }
    Region::decode(buf)
}

/// Parsed submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Call {
    id: u64,
    op: Op,
}

impl Call {
    pub const fn new(id: u64, op: Op) -> Self {
        Self { id, op }
    }

    /// The token the completion carries back.
    pub const fn id(self) -> u64 {
        self.id
    }

    pub const fn op(self) -> Op {
        self.op
    }

    /// Parses a slot the kernel has already copied out of shared memory.
    ///
    /// By value, so there is no shared page in scope to fetch a field from
    /// twice. Not `const`: the region check ends in a mask the optimizer must
    /// not see through, and a const evaluator cannot have an optimizer barrier
    /// ([`nospec`](crate::nospec)).
    pub fn parse(words: [u64; SLOT_WORDS]) -> Result<Self, Reject> {
        let (id, tag) = (words[0], words[1] as u32);
        // High half of tag and tail words are reserved for future fields.
        if (words[1] >> 32) != 0 || words[5] != 0 || words[6] != 0 || words[7] != 0 {
            return Err(Reject::Reserved);
        }

        let (first, second, third) = (words[2], words[3], words[4]);
        let cap = Handle::new(first);
        let op = match tag {
            1 => Op::Read { cap, offset: second, buf: Region::decode(third)? },
            2 => Op::Write { cap, offset: second, buf: Region::decode(third)? },
            3 => zeroed([second, third], Op::Flush { cap })?,
            4 => Op::Open { dir: cap, name: payload(second, third)? },
            5 => zeroed([second, third], Op::Close { cap })?,
            6 => Op::Send { cap, buf: payload(second, third)? },
            7 => Op::Recv { cap, buf: payload(second, third)? },
            8 => zeroed([second, third], Op::Timer { ticks: first })?,
            9 => Op::Message { channel: cap, buf: payload(second, third)? },
            10 => {
                let rights = third as u32;
                zeroed([third >> 32], Op::Grant { channel: cap, cap: Handle::new(second), rights })?
            }
            _ => return Err(Reject::Tag),
        };
        Ok(Self { id, op })
    }

    pub const fn encode(self) -> [u64; SLOT_WORDS] {
        let [first, second, third] = self.op.words();
        [self.id, self.op.tag() as u64, first, second, third, 0, 0, 0]
    }
}

/// Completion reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reply {
    id: u64,
    result: i64,
}

impl Reply {
    pub const fn new(id: u64, result: i64) -> Self {
        Self { id, result }
    }

    /// Completion for rejected submission.
    pub const fn rejected(id: u64, reject: Reject) -> Self {
        Self { id, result: -(reject as i64) }
    }

    pub const fn id(self) -> u64 {
        self.id
    }

    pub const fn result(self) -> i64 {
        self.result
    }

    pub const fn encode(self) -> [u64; SLOT_WORDS] {
        [self.id, self.result as u64, 0, 0, 0, 0, 0, 0]
    }

    pub const fn decode(words: [u64; SLOT_WORDS]) -> Self {
        Self { id: words[0], result: words[1] as i64 }
    }
}
