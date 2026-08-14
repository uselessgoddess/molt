//! What a domain that lies about its ring is allowed to do to the kernel.
//!
//! The tests next door name one lie each; these name none. Proptest generates
//! submissions, bends some a bit at a time, and moves the shared indices
//! wherever it likes. The kernel side must read exactly what was written into
//! the slot it owns or fault, and must fault only on a ring no honest producer
//! could have published.

use churn::{MOVES, Seen, sweep};
use molt_abi::wire::APERTURE;
use molt_abi::{Call, Channel, Fault, Handle, Next, Op, Region, Reply, SLOT_WORDS};
use proptest::prelude::*;
use proptest::sample::Index;

/// Four slots, so a couple of dozen moves wrap it many times over and a
/// producer running ahead is a state the sweep reaches on its own.
const RING: usize = 4;

/// A buffer, half inside the aperture and half anywhere: two random `u32`s land
/// past 4 GiB about half the time, which reaches the refusal often and anything
/// behind it rarely.
fn region() -> impl Strategy<Value = Region> {
    prop_oneof![
        1 => (0..1u32 << 31, 0..1u32 << 31).prop_map(|(offset, len)| Region::new(offset, len)),
        1 => (any::<u32>(), any::<u32>()).prop_map(|(offset, len)| Region::new(offset, len)),
    ]
}

fn handle() -> impl Strategy<Value = Handle> {
    any::<u64>().prop_map(Handle::new)
}

/// Every operation the wire has a tag for, so no arm of the parser is reached
/// only by a bent slot.
fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (handle(), any::<u64>(), region()).prop_map(|(cap, offset, buf)| Op::Read {
            cap,
            offset,
            buf
        }),
        (handle(), any::<u64>(), region()).prop_map(|(cap, offset, buf)| Op::Write {
            cap,
            offset,
            buf
        }),
        handle().prop_map(|cap| Op::Flush { cap }),
        (handle(), region()).prop_map(|(dir, name)| Op::Open { dir, name }),
        handle().prop_map(|cap| Op::Close { cap }),
        (handle(), region()).prop_map(|(cap, buf)| Op::Send { cap, buf }),
        (handle(), region()).prop_map(|(cap, buf)| Op::Recv { cap, buf }),
        any::<u64>().prop_map(|ticks| Op::Timer { ticks }),
        (handle(), region()).prop_map(|(channel, buf)| Op::Message { channel, buf }),
        (handle(), handle(), any::<u32>()).prop_map(|(channel, cap, rights)| Op::Grant {
            channel,
            cap,
            rights
        }),
    ]
}

fn call() -> impl Strategy<Value = Call> {
    (any::<u64>(), op()).prop_map(|(id, op)| Call::new(id, op))
}

/// One turn by the domain against the submission ring.
#[derive(Clone, Copy, Debug)]
enum Move {
    /// A submission the domain encodes itself, as the library would.
    Submit(Call),
    /// The same with one bit turned over: mostly a call the parser refuses for
    /// a named reason, sometimes a different call. The sweep cares only that
    /// the kernel reads the slot it owns.
    Bend { call: Call, word: Index, bit: u8 },
    /// The tail moved without anything being written: zero slots ahead, one, or
    /// more than the ring holds.
    Claim(u32),
    /// The kernel side reading one slot.
    Take,
}

fn moves() -> impl Strategy<Value = Vec<Move>> {
    let turn = prop_oneof![
        3 => call().prop_map(Move::Submit),
        3 => (call(), any::<Index>(), 0..64u8)
            .prop_map(|(call, word, bit)| Move::Bend { call, word, bit }),
        1 => (0..9u32).prop_map(Move::Claim),
        4 => Just(Move::Take),
    ];
    prop::collection::vec(turn, MOVES)
}

/// What the kernel must hand up for a slot holding `words`, whoever wrote them.
fn decided(words: [u64; SLOT_WORDS]) -> Next {
    match Call::parse(words) {
        Ok(call) => Next::Ready(call),
        Err(reject) => Next::Rejected { id: words[0], reject },
    }
}

#[test]
fn lying_producer_reaches_fault() {
    let seen = Seen::default();

    sweep(moves(), |moves| {
        let channel = Channel::<RING>::new();
        let (mut submissions, _) = channel.kernel();
        let mut domain = channel.domain();
        // What the sweep knows: the bytes in each slot, and where an honest
        // producer's indices would be.
        let mut wrote = [[0; SLOT_WORDS]; RING];
        let mut tail = 0u32;
        let mut head = 0u32;
        let mut faulted = false;

        for turn in moves {
            match turn {
                Move::Submit(call) => {
                    wrote[tail as usize % RING] = call.encode();
                    domain.submit(call);
                    tail = tail.wrapping_add(1);
                }
                Move::Bend { call, word, bit } => {
                    let mut words = call.encode();
                    words[word.index(SLOT_WORDS)] ^= 1 << bit;
                    wrote[tail as usize % RING] = words;
                    domain.write(words);
                    tail = tail.wrapping_add(1);
                }
                Move::Claim(ahead) => {
                    domain.claim(ahead);
                    tail = tail.wrapping_add(ahead);
                }
                Move::Take => {
                    let ready = tail.wrapping_sub(head);
                    let slot = wrote[head as usize % RING];

                    match submissions.take() {
                        Err(fault) => {
                            prop_assert_eq!(fault, Fault::Tail);
                            prop_assert!(
                                faulted || ready as usize > RING,
                                "an honest ring faulted"
                            );
                            faulted = true;
                            seen.saw("tail past the slots the ring has");
                        }
                        Ok(Next::Empty) => {
                            prop_assert!(!faulted, "a faulted ring answered as a queue again");
                            prop_assert_eq!(ready, 0, "a published submission was read as nothing");
                            seen.saw("ring with nothing in it");
                        }
                        Ok(next) => {
                            prop_assert!(!faulted, "a faulted ring handed up a submission");
                            prop_assert!(
                                ready as usize <= RING,
                                "more submissions than slots was read"
                            );
                            prop_assert_eq!(next, decided(slot), "a slot the kernel does not own");
                            match next {
                                Next::Ready(call) => {
                                    let buf = call.op().region();
                                    prop_assert!(
                                        buf.is_none_or(|buf| buf.fits(APERTURE)),
                                        "an accepted call named a buffer outside the aperture"
                                    );
                                    prop_assert_eq!(
                                        Call::parse(call.encode()),
                                        Ok(call),
                                        "an accepted call does not survive its own encoding"
                                    );
                                    seen.saw("submission the kernel accepted");
                                }
                                _ => seen.saw("submission the kernel refused"),
                            }
                            head = head.wrapping_add(1);
                        }
                    }
                    prop_assert_eq!(submissions.taken(), head, "the private head moved on its own");
                }
            }
        }
        Ok(())
    });

    seen.reached(&[
        "submission the kernel accepted",
        "submission the kernel refused",
        "ring with nothing in it",
        "tail past the slots the ring has",
    ]);
}

/// One turn by either end against the completion ring.
#[derive(Clone, Copy, Debug)]
enum Answer {
    /// The kernel publishing a reply, which the ring may have no room for.
    Publish(Reply),
    /// The domain taking one.
    Take,
    /// The domain saying it consumed replies nobody published, which is the lie
    /// this half is named after.
    Consumed(u32),
}

fn answers() -> impl Strategy<Value = Vec<Answer>> {
    let turn = prop_oneof![
        7 => (any::<u64>(), any::<i64>()).prop_map(|(id, result)| Answer::Publish(Reply::new(id, result))),
        8 => Just(Answer::Take),
        1 => (1..=4u32).prop_map(Answer::Consumed),
    ];
    prop::collection::vec(turn, MOVES)
}

#[test]
fn lies_its_head_starves_only_itself() {
    let seen = Seen::default();

    sweep(answers(), |answers| {
        let channel = Channel::<RING>::new();
        let (_, mut completions) = channel.kernel();
        let mut domain = channel.domain();
        let mut sent: Vec<Reply> = Vec::new();
        let mut corrupted = false;

        for turn in answers {
            match turn {
                Answer::Publish(reply) => match completions.publish(reply) {
                    Ok(()) => {
                        prop_assert!(
                            corrupted || sent.len() < RING,
                            "the ring took a reply too many"
                        );
                        sent.push(reply);
                        seen.saw("reply published");
                    }
                    Err(back) => {
                        prop_assert_eq!(back, reply, "a refused completion was swallowed");
                        prop_assert!(corrupted || sent.len() == RING, "a ring with room refused");
                        seen.saw("completion ring with no room left");
                    }
                },
                Answer::Take => {
                    let taken = domain.reply();
                    if !corrupted {
                        prop_assert_eq!(
                            taken,
                            sent.first().copied(),
                            "an honest domain lost a reply"
                        );
                        if taken.is_some() {
                            sent.remove(0);
                            seen.saw("reply taken");
                        }
                    }
                }
                Answer::Consumed(ahead) => {
                    domain.consumed(ahead);
                    corrupted = true;
                    seen.saw("head the domain moved for itself");
                }
            }
        }
        Ok(())
    });

    seen.reached(&[
        "reply published",
        "reply taken",
        "completion ring with no room left",
        "head the domain moved for itself",
    ]);
}
