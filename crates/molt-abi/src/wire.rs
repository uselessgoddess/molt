//! What a submission is on the wire, and what it parses into.
//!
//! A slot is a fixed number of little words with no invalid bit pattern, and a
//! [`Call`] is what one of them means once it has been checked. The direction
//! matters: the kernel never has a `Call` it did not build out of bytes it had
//! already copied, so there is no field left for the other end to change after
//! the check.

use crate::nospec;

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
    /// The architectural answer, and the only thing here worth branching on:
    /// what the branch then carries has to come from [`within`], because that
    /// is what keeps the refusal true while the branch is still a guess.
    ///
    /// [`within`]: Region::within
    #[inline(always)]
    pub fn fits(self, bytes: u64) -> bool {
        self.inside(bytes).passed()
    }

    /// This region masked against `bytes`: itself when it fits, and the empty
    /// region at offset zero when it does not.
    ///
    /// The value to hand to the load, whether or not [`fits`] has been asked
    /// yet — that is the whole of the Spectre-v1 defence, and
    /// [`nospec`] says why it is shaped this way.
    ///
    /// [`fits`]: Region::fits
    /// [`nospec`]: crate::nospec
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
    fn inside(self, bytes: u64) -> nospec::Mask {
        nospec::upto(self.offset as u64 + self.len as u64, bytes)
    }

    /// Reads a region out of one wire word, rejecting one outside the aperture.
    ///
    /// The mask is applied above the branch and carried through it, so the
    /// region an `Ok` hands on is masked on the path the processor guessed as
    /// well as the one it takes.
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
    /// no shared page in scope here to fetch a field from twice.
    ///
    /// Not `const`: the region check ends in a mask the optimizer must not see
    /// through, and an optimizer barrier is the one thing a const evaluator
    /// cannot have ([`nospec`]).
    ///
    /// [`nospec`]: crate::nospec
    pub fn parse(words: [u64; SLOT_WORDS]) -> Result<Self, Reject> {
        let (id, tag) = (words[0], words[1] as u32);
        // High half of tag and tail words are reserved for future fields.
        if (words[1] >> 32) != 0 || words[5] != 0 || words[6] != 0 || words[7] != 0 {
            return Err(Reject::Reserved);
        }

        let (first, second, third) = (words[2], words[3], words[4]);
        let op = match tag {
            1 | 2 => {
                let buf = Region::decode(third)?;
                let (cap, offset) = (Handle::new(first), second);
                if tag == 1 {
                    Op::Read { cap, offset, buf }
                } else {
                    Op::Write { cap, offset, buf }
                }
            }
            3 | 5 | 8 => {
                if second != 0 || third != 0 {
                    return Err(Reject::Reserved);
                }
                match tag {
                    3 => Op::Flush { cap: Handle::new(first) },
                    5 => Op::Close { cap: Handle::new(first) },
                    _ => Op::Timer { ticks: first },
                }
            }
            4 | 6 | 7 | 9 => {
                if third != 0 {
                    return Err(Reject::Reserved);
                }
                let buf = Region::decode(second)?;
                let cap = Handle::new(first);
                match tag {
                    4 => Op::Open { dir: cap, name: buf },
                    6 => Op::Send { cap, buf },
                    7 => Op::Recv { cap, buf },
                    _ => Op::Message { channel: cap, buf },
                }
            }
            10 => {
                if third >> 32 != 0 {
                    return Err(Reject::Reserved);
                }
                Op::Grant {
                    channel: Handle::new(first),
                    cap: Handle::new(second),
                    rights: third as u32,
                }
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
