//! A producer that writes whatever it likes, for as long as it likes.
//!
//! The tests next door play one lie each, chosen because somebody thought of
//! it. This one plays every lie a seeded xorshift can put together — tails
//! nobody earned, slots rewritten under the kernel's hands, submissions one bit
//! away from real ones, a head the domain never reached — and holds the kernel
//! end to the same claims after each of them.

use molt_abi::wire::APERTURE;
use molt_abi::{Call, Channel, Fault, Handle, Next, Op, Region, Reply, SLOT_WORDS};

const ROUNDS: usize = 1 << 10;
/// Lies per round. A fault ends a ring for good, so a round is a fresh channel
/// and this is how much rope each producer gets before the next one starts.
const MOVES: usize = 24;
const SEED: u64 = 0x6d6f_6c74_7269_6e67;
/// Four slots, so a round fills, wraps and overruns the ring several times.
const RING: usize = 4;

struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A submission of some shape, whose fields are whatever the noise says:
    /// most tags carry a region, and a region built from two random halves is
    /// past the aperture far more often than not.
    fn call(&mut self) -> Call {
        let cap = Handle::new(self.next());
        let buf = Region::new(self.next() as u32, self.next() as u32);
        let op = match self.next() % 6 {
            0 => Op::Read { cap, offset: self.next(), buf },
            1 => Op::Write { cap, offset: self.next(), buf },
            2 => Op::Open { dir: cap, name: buf },
            3 => Op::Timer { ticks: self.next() },
            4 => Op::Grant { channel: cap, cap, rights: self.next() as u32 },
            _ => Op::Message { channel: cap, buf },
        };
        Call::new(self.next(), op)
    }
}

/// What a slot says, decided the way the kernel has to decide it: parse the
/// copy once, and a parse that fails still costs the submission.
fn decided(words: [u64; SLOT_WORDS]) -> Next {
    match Call::parse(words) {
        Ok(call) => Next::Ready(call),
        Err(reject) => Next::Rejected { id: words[0], reject },
    }
}

#[test]
fn a_lying_producer_reaches_a_fault_and_no_further() {
    let mut noise = Noise(SEED);
    let mut counts = [0; 3];

    for _ in 0..ROUNDS {
        let channel = Channel::<RING>::new();
        let (mut submissions, _) = channel.kernel();
        let mut domain = channel.domain();
        // The producer's tail and what it has left in each slot, as the test
        // knows them. The kernel is not allowed to answer with anything else.
        let mut wrote = [[0; SLOT_WORDS]; RING];
        let mut tail = 0u32;
        let mut head = 0u32;
        let mut faulted = false;

        for _ in 0..MOVES {
            match noise.next() % 8 {
                0 | 1 => {
                    let call = noise.call();
                    wrote[tail as usize % RING] = call.encode();
                    domain.submit(call);
                    tail = tail.wrapping_add(1);
                }
                2 | 3 => {
                    // A slot one bit away from a submission the kernel would
                    // have run, which is the shape a parser is worst at.
                    let mut words = noise.call().encode();
                    words[noise.next() as usize % SLOT_WORDS] ^= 1 << (noise.next() % 64);
                    wrote[tail as usize % RING] = words;
                    domain.write(words);
                    tail = tail.wrapping_add(1);
                }
                4 => {
                    // A tail nothing was written for, including one walked
                    // backwards past the kernel's head.
                    let ahead = noise.next() as u32 % 9;
                    domain.claim(ahead);
                    tail = tail.wrapping_add(ahead);
                }
                _ => {
                    let ready = tail.wrapping_sub(head);
                    let slot = wrote[head as usize % RING];

                    match submissions.take() {
                        Err(fault) => {
                            assert_eq!(fault, Fault::Tail);
                            assert!(faulted || ready as usize > RING, "an honest ring faulted");
                            faulted = true;
                            counts[0] += 1;
                        }
                        Ok(Next::Empty) => {
                            assert!(!faulted, "a faulted ring answered as a queue again");
                            assert_eq!(ready, 0, "a published submission was read as nothing");
                        }
                        Ok(next) => {
                            assert!(!faulted, "a faulted ring handed up a submission");
                            assert!(ready as usize <= RING, "more submissions than slots was read");
                            assert_eq!(next, decided(slot), "a slot the kernel does not own");
                            if let Next::Ready(call) = next {
                                let buf = call.op().region();
                                assert!(
                                    buf.is_none_or(|buf| buf.within(APERTURE).is_some()),
                                    "an accepted call named a buffer outside the aperture"
                                );
                                counts[1] += 1;
                            } else {
                                counts[2] += 1;
                            }
                            head = head.wrapping_add(1);
                        }
                    }
                    assert_eq!(submissions.taken(), head, "the private head moved on its own");
                }
            }
        }
    }

    assert!(counts.iter().all(|&count| count > 0), "the sweep never got to something: {counts:?}");
}

#[test]
fn a_domain_that_lies_about_its_head_starves_only_itself() {
    let mut noise = Noise(SEED);
    let mut counts = [0; 3];

    for _ in 0..ROUNDS {
        let channel = Channel::<RING>::new();
        let (_, mut completions) = channel.kernel();
        let mut domain = channel.domain();
        // Published and not yet read, in the order the kernel published them.
        let mut sent: Vec<Reply> = Vec::new();
        let mut corrupted = false;

        for _ in 0..MOVES {
            match noise.next() % 16 {
                0..=6 => {
                    let reply = Reply::new(noise.next(), noise.next() as i64);
                    match completions.publish(reply) {
                        Ok(()) => {
                            assert!(
                                corrupted || sent.len() < RING,
                                "the ring took a reply too many"
                            );
                            sent.push(reply);
                        }
                        Err(back) => {
                            assert_eq!(back, reply, "a refused completion was swallowed");
                            assert!(corrupted || sent.len() == RING, "a ring with room refused");
                            counts[0] += 1;
                        }
                    }
                }
                7..=14 => {
                    let taken = domain.reply();
                    // A domain that lied about its own head reads whatever that
                    // leaves it pointing at. Rule 6: its memory, its problem,
                    // and the kernel end above carries on regardless.
                    if !corrupted {
                        assert_eq!(taken, sent.first().copied(), "an honest domain lost a reply");
                        if taken.is_some() {
                            sent.remove(0);
                            counts[1] += 1;
                        }
                    }
                }
                _ => {
                    domain.consumed(1 + noise.next() as u32 % 4);
                    corrupted = true;
                    counts[2] += 1;
                }
            }
        }
    }

    assert!(counts.iter().all(|&count| count > 0), "the sweep never got to something: {counts:?}");
}
