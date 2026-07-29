//! Wire parsers meeting bytes nobody wrote.
//!
//! A generator rather than a corpus: the seed is the reproduction, so a failure
//! replays from one constant in the source and needs no checked-in inputs, no
//! crash triage, and no time budget in CI.
//!
//! The noise is shaped before it is parsed. A random checksum, version nibble,
//! or fragment field stops every input at the same early return, and the length
//! arithmetic beneath it — the part that indexes — is what has to hold.

use molt_net::addr::{Ipv4Addr, Ipv6Addr};
use molt_net::icmpv6::Message;
use molt_net::ipv4::Packet as Ipv4;
use molt_net::ipv6::Packet as Ipv6;
use molt_net::{NetError, checksum, icmpv6};

const ROUNDS: usize = 1 << 14;
const SEED: u64 = 0x6d6f_6c74_6e65_7400;
const LOCAL: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const PEER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const LOCAL_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1);
const PEER_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2);

/// The ICMPv6 types this stack answers, which noise would otherwise never hit.
const TYPES: [u8; 4] = [128, 129, 135, 136];

/// xorshift64*, chosen because one constant names an entire run.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Writes a prefix of `bytes`, up to and including the whole of it.
    fn fill(&mut self, bytes: &mut [u8]) -> usize {
        let len = self.next() as usize % (bytes.len() + 1);
        for byte in &mut bytes[..len] {
            *byte = self.next() as u8;
        }
        len
    }

    /// A length field that lands inside the buffer about as often as past it.
    fn length(&mut self) -> u16 {
        (self.next() % 160) as u16
    }

    /// Flips one bit of `bytes`.
    fn flip(&mut self, bytes: &mut [u8]) {
        let at = self.next() as usize % bytes.len();
        bytes[at] ^= 1 << (self.next() % 8);
    }
}

#[test]
fn ipv4_noise_stays_inside_input() {
    let mut noise = Noise(SEED);
    let mut storage = [0u8; 128];
    let mut accepted = 0;

    for _ in 0..ROUNDS {
        let len = noise.fill(&mut storage);
        let bytes = &mut storage[..len];
        if bytes.len() < Ipv4::HEADER {
            continue;
        }
        bytes[0] = 0x40 | (noise.next() as u8 & 0x0f);
        bytes[2..4].copy_from_slice(&noise.length().to_be_bytes());
        bytes[6..8].fill(0);
        let header = ((bytes[0] & 0x0f) as usize) * 4;
        if (Ipv4::HEADER..=bytes.len()).contains(&header) {
            checksum::set(&mut bytes[..header], 10);
        }

        if let Ok(packet) = Ipv4::parse(bytes) {
            assert!(packet.payload().len() < bytes.len(), "payload outgrew its packet");
            accepted += 1;
        }
    }

    assert!(accepted > ROUNDS / 100, "the sweep proved nothing: {accepted} inputs parsed");
}

#[test]
fn ipv6_noise_stays_inside_input() {
    let mut noise = Noise(SEED);
    let mut storage = [0u8; 128];
    let mut accepted = 0;

    for _ in 0..ROUNDS {
        let len = noise.fill(&mut storage);
        let bytes = &mut storage[..len];
        if bytes.len() < Ipv6::HEADER {
            continue;
        }
        bytes[0] = 0x60;
        bytes[4..6].copy_from_slice(&noise.length().to_be_bytes());

        if let Ok(packet) = Ipv6::parse(bytes) {
            assert!(packet.payload().len() < bytes.len(), "payload outgrew its packet");
            accepted += 1;
        }
    }

    assert!(accepted > ROUNDS / 100, "the sweep proved nothing: {accepted} inputs parsed");
}

#[test]
fn icmpv6_noise_stays_inside_input() {
    let mut noise = Noise(SEED);
    let mut storage = [0u8; 128];
    let mut accepted = 0;

    for _ in 0..ROUNDS {
        let len = noise.fill(&mut storage);
        let bytes = &mut storage[..len];
        if bytes.len() < 8 {
            continue;
        }
        bytes[0] = TYPES[noise.next() as usize % TYPES.len()];
        bytes[1] = 0;
        bytes[2..4].fill(0);
        let sum = checksum::over_ipv6(PEER_V6, LOCAL_V6, icmpv6::PROTOCOL, bytes);
        bytes[2..4].copy_from_slice(&sum.to_be_bytes());

        if let Ok(message) = Message::parse(PEER_V6, LOCAL_V6, bytes) {
            assert!(message.bytes() <= bytes.len(), "message outgrew its packet");
            accepted += 1;
        }
    }

    assert!(accepted > ROUNDS / 100, "the sweep proved nothing: {accepted} inputs parsed");
}

#[test]
fn mutated_ipv4_reemits_itself() -> Result<(), NetError> {
    let mut noise = Noise(SEED);
    let mut valid = [0u8; 64];
    let len = Ipv4::new(LOCAL, PEER, 17, b"datagram").emit(&mut valid)?;
    let mut accepted = 0;

    for _ in 0..ROUNDS {
        let mut mutated = valid;
        noise.flip(&mut mutated[..len]);
        let Ok(parsed) = Ipv4::parse(&mutated[..len]) else { continue };

        let mut again = [0u8; 64];
        let emitted = parsed.emit(&mut again)?;
        assert_eq!(Ipv4::parse(&again[..emitted]), Ok(parsed), "a packet it cannot rewrite");
        accepted += 1;
    }

    assert!(accepted > ROUNDS / 100, "the sweep proved nothing: {accepted} mutations parsed");
    Ok(())
}

#[test]
fn mutated_ipv6_reemits_itself() -> Result<(), NetError> {
    let mut noise = Noise(SEED);
    let mut valid = [0u8; 64];
    let len = Ipv6::new(LOCAL_V6, PEER_V6, 17, b"datagram").emit(&mut valid)?;
    let mut accepted = 0;

    for _ in 0..ROUNDS {
        let mut mutated = valid;
        noise.flip(&mut mutated[..len]);
        let Ok(parsed) = Ipv6::parse(&mutated[..len]) else { continue };

        let mut again = [0u8; 64];
        let emitted = parsed.emit(&mut again)?;
        assert_eq!(Ipv6::parse(&again[..emitted]), Ok(parsed), "a packet it cannot rewrite");
        accepted += 1;
    }

    assert!(accepted > ROUNDS / 100, "the sweep proved nothing: {accepted} mutations parsed");
    Ok(())
}
