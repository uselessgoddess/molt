//! What a submission is on the wire, and what it parses into.
//!
//! A slot is a fixed number of little words with no invalid bit pattern, and a
//! [`Call`] is what one of them means once it has been checked. The direction
//! matters: the kernel never has a `Call` it did not build out of bytes it had
//! already copied, so there is no field left for the other end to change after
//! the check.

/// Bytes in one submission or completion slot, every operation the same size.
///
/// Fixed because the arms are a union and not a tagged encoding: a slot's
/// position is arithmetic, and a producer cannot move the next one by lying
/// about the length of this one.
pub const SLOT_BYTES: usize = 64;

/// Words the kernel reads a slot as.
pub const SLOT_WORDS: usize = SLOT_BYTES / 8;

/// The tier-1 aperture every offset in a submission is measured inside.
///
/// A [`Region`] is two `u32`s because the aperture is 4 GiB, so the check that
/// one lies inside it is a single unsigned compare that cannot wrap once it is
/// done in `u64`.
pub const APERTURE: u64 = 1 << 32;

/// A capability named by a submission.
///
/// The integer is not the authority; the submitting domain's own table is. Two
/// domains writing the same number name two different capabilities, which is
/// why copying one buys nothing.
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

    /// The offset this region starts at inside an extent `bytes` long, or
    /// `None` when it does not fit.
    ///
    /// This is the one range check the fast path is allowed, and it is sound
    /// only because `self` is already a copy: the domain cannot change the
    /// offset between this check and the use it authorises.
    ///
    /// It is also the textbook Spectre-v1 gadget — a bounds check on an
    /// attacker-chosen index followed immediately by the use — so the answer is
    /// masked rather than merely branched on. A misprediction that runs the
    /// in-range path anyway gets offset zero, which is inside every extent,
    /// instead of an address outside it that would leave a cache trace. Masking
    /// and not a fence, because RISC-V has no ratified speculation barrier to
    /// reach for and this needs none.
    pub const fn within(self, bytes: u64) -> Option<u64> {
        // Both `u32`, so the sum is exact in `u64` and a length cannot wrap the
        // end back inside the extent.
        let end = self.offset as u64 + self.len as u64;
        let inside = fits(end, bytes);
        if inside == 0 { None } else { Some(self.offset as u64 & inside) }
    }

    /// Reads a region out of one wire word, rejecting one outside the aperture.
    const fn decode(word: u64) -> Result<Self, Reject> {
        let region = Self { offset: word as u32, len: (word >> 32) as u32 };
        match region.within(APERTURE) {
            Some(_) => Ok(region),
            None => Err(Reject::Region),
        }
    }

    const fn encode(self) -> u64 {
        self.offset as u64 | (self.len as u64) << 32
    }
}

/// All ones when `value <= limit`, zero otherwise, computed without a branch.
///
/// The subtraction borrows exactly when `value` is past `limit`, and the
/// arithmetic shift smears that sign bit across the word. Both sides stay below
/// `1 << 63` — an offset is 32-bit and no extent is exabytes — so a borrow is
/// the only thing that can set the top bit.
const fn fits(value: u64, limit: u64) -> u64 {
    !(((limit.wrapping_sub(value) as i64) >> 63) as u64)
}

/// One operation, as the kernel has it after parsing.
///
/// The wire form is the `u32` tag below and three fixed-size words, so adding
/// an operation is a version change both ends can see rather than a number
/// nobody notices.
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

/// Why one submission was not run, when the ring itself is still honest.
///
/// A rejection costs the domain that submission and nothing else: it gets a
/// completion saying so, and the ring keeps going. Compare [`Fault`], which is
/// the ring lying about how much it holds and ends the conversation.
///
/// [`Fault`]: crate::ring::Fault
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Reject {
    /// A tag this kernel does not implement, including the reserved zero.
    Tag = 1,
    /// A word the ABI declares zero was not zero. A future field lives there,
    /// and an old program that left junk in it would silently mean something.
    Reserved = 2,
    /// A buffer that leaves the aperture.
    Region = 3,
}

/// One submission, parsed.
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
    /// Taking the words by value is the read-once rule as a signature: there is
    /// no shared page in scope here to fetch a field from twice.
    pub const fn parse(words: [u64; SLOT_WORDS]) -> Result<Self, Reject> {
        let (id, tag) = (words[0], words[1] as u32);
        // The high half of the tag word, and everything past the arms, is where
        // the next version's fields go.
        if (words[1] >> 32) != 0 || words[5] != 0 || words[6] != 0 || words[7] != 0 {
            return Err(Reject::Reserved);
        }

        let (first, second, third) = (words[2], words[3], words[4]);
        let op = match tag {
            1 | 2 => {
                let buf = match Region::decode(third) {
                    Ok(buf) => buf,
                    Err(reject) => return Err(reject),
                };
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
                let buf = match Region::decode(second) {
                    Ok(buf) => buf,
                    Err(reject) => return Err(reject),
                };
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

    /// Writes this call out as a slot, which is what the other end does.
    pub const fn encode(self) -> [u64; SLOT_WORDS] {
        let [first, second, third] = self.op.words();
        [self.id, self.op.tag() as u64, first, second, third, 0, 0, 0]
    }
}

/// One completion, on its way back to the domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Reply {
    id: u64,
    result: i64,
}

impl Reply {
    /// A completion for `id`, where a negative `result` is an error number.
    pub const fn new(id: u64, result: i64) -> Self {
        Self { id, result }
    }

    /// The completion a rejected submission earns, so the domain learns why
    /// rather than waiting for an answer that is never coming.
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

    /// Reads a completion back, which is what the domain does.
    pub const fn decode(words: [u64; SLOT_WORDS]) -> Self {
        Self { id: words[0], result: words[1] as i64 }
    }
}
