use molt_churn::{Seen, sweep};
use molt_net::icmpv6::Message;
use molt_net::ipv4::Packet as Ipv4;
use molt_net::ipv6::Packet as Ipv6;
use molt_net::{Ipv4Addr, Ipv6Addr, checksum, icmpv6};
use proptest::prelude::*;
use proptest::sample::Index;

const FRAME: usize = 128;
const LOCAL: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const PEER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const LOCAL_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1);
const PEER_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2);

/// The ICMPv6 types this stack answers, which bytes nobody shaped would reach
/// once in a few million frames.
const TYPES: [u8; 4] = [128, 129, 135, 136];

/// A frame of at least `least` bytes.
fn frame(least: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), least..=FRAME)
}

/// A length field, in the range where it lands inside a frame often enough to
/// generate. Longer than any frame here and every packet is refused
/// for the same reason.
fn length() -> impl Strategy<Value = u16> {
    0..160u16
}

/// One bit of a frame, as a position and which bit.
fn flip() -> impl Strategy<Value = (Index, u8)> {
    (any::<Index>(), 0..8u8)
}

#[test]
fn ipv4_stays_inside_input() {
    let seen = Seen::default();

    sweep((frame(Ipv4::HEADER), any::<u8>(), length()), |(mut bytes, ihl, length)| {
        // A version nibble and a header length the parser will read: without
        // them nothing is ever accepted, and refusals alone prove nothing about
        // what the payload of an accepted packet points at.
        bytes[0] = 0x40 | (ihl & 0x0f);
        bytes[2..4].copy_from_slice(&length.to_be_bytes());
        bytes[6..8].fill(0);
        let header = ((bytes[0] & 0x0f) as usize) * 4;
        if (Ipv4::HEADER..=bytes.len()).contains(&header) {
            checksum::set(&mut bytes[..header], 10);
        }

        if let Ok(packet) = Ipv4::parse(&bytes) {
            prop_assert!(packet.payload().len() < bytes.len(), "payload outgrew its packet");
            seen.saw("an IPv4 packet parsed");
        }
        Ok(())
    });

    seen.reached(&["an IPv4 packet parsed"]);
}

#[test]
fn ipv6_stays_inside_input() {
    let seen = Seen::default();

    sweep((frame(Ipv6::HEADER), length()), |(mut bytes, length)| {
        bytes[0] = 0x60;
        bytes[4..6].copy_from_slice(&length.to_be_bytes());

        if let Ok(packet) = Ipv6::parse(&bytes) {
            prop_assert!(packet.payload().len() < bytes.len(), "payload outgrew its packet");
            seen.saw("an IPv6 packet parsed");
        }
        Ok(())
    });

    seen.reached(&["an IPv6 packet parsed"]);
}

#[test]
fn icmpv6_stays_inside_input() {
    let seen = Seen::default();

    sweep((frame(8), any::<Index>()), |(mut bytes, kind)| {
        bytes[0] = *kind.get(&TYPES);
        bytes[1] = 0;
        // The checksum covers the whole message, so a frame with a wrong one is
        // refused before anything else is looked at.
        bytes[2..4].fill(0);
        let sum = checksum::over_ipv6(PEER_V6, LOCAL_V6, icmpv6::PROTOCOL, &bytes);
        bytes[2..4].copy_from_slice(&sum.to_be_bytes());

        if let Ok(message) = Message::parse(PEER_V6, LOCAL_V6, &bytes) {
            prop_assert!(message.bytes() <= bytes.len(), "message outgrew its packet");
            seen.saw("an ICMPv6 message parsed");
        }
        Ok(())
    });

    seen.reached(&["an ICMPv6 message parsed"]);
}

#[test]
fn mutated_ipv4_reemits_itself() {
    let seen = Seen::default();
    let mut valid = [0u8; 64];
    let len = Ipv4::new(LOCAL, PEER, 17, b"datagram").emit(&mut valid).expect("a datagram fits");

    sweep(flip(), |(at, bit)| {
        let mut mutated = valid;
        mutated[at.index(len)] ^= 1 << bit;
        let Ok(parsed) = Ipv4::parse(&mutated[..len]) else { return Ok(()) };

        let mut again = [0u8; 64];
        let emitted = parsed.emit(&mut again).expect("what parsed fits where it came from");
        prop_assert_eq!(Ipv4::parse(&again[..emitted]), Ok(parsed), "a packet it cannot rewrite");
        seen.saw("a mutated IPv4 packet parsed");
        Ok(())
    });

    seen.reached(&["a mutated IPv4 packet parsed"]);
}

#[test]
fn mutated_ipv6_reemits_itself() {
    let seen = Seen::default();
    let mut valid = [0u8; 64];
    let len =
        Ipv6::new(LOCAL_V6, PEER_V6, 17, b"datagram").emit(&mut valid).expect("a datagram fits");

    sweep(flip(), |(at, bit)| {
        let mut mutated = valid;
        mutated[at.index(len)] ^= 1 << bit;
        let Ok(parsed) = Ipv6::parse(&mutated[..len]) else { return Ok(()) };

        let mut again = [0u8; 64];
        let emitted = parsed.emit(&mut again).expect("what parsed fits where it came from");
        prop_assert_eq!(Ipv6::parse(&again[..emitted]), Ok(parsed), "a packet it cannot rewrite");
        seen.saw("a mutated IPv6 packet parsed");
        Ok(())
    });

    seen.reached(&["a mutated IPv6 packet parsed"]);
}
