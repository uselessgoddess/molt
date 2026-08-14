#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use molt_net::icmpv6::Message;
use molt_net::ipv4::Packet as Ipv4;
use molt_net::ipv6::Packet as Ipv6;
use molt_net::{Ipv6Addr, arp, checksum, eth, icmpv6};

const LOCAL_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1);
const PEER_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2);

#[derive(Arbitrary, Debug)]
enum Frame<'a> {
    Eth(&'a [u8]),
    Arp(&'a [u8]),
    Ipv4(&'a [u8]),
    Ipv6(&'a [u8]),
    Icmpv6 { checked: bool, bytes: &'a [u8] },
}

fuzz_target!(|frame: Frame| {
    match frame {
        Frame::Eth(bytes) => {
            if let Ok(parsed) = eth::Frame::parse(bytes) {
                assert!(parsed.payload().len() < bytes.len(), "payload outgrew its frame");
                let mut again = vec![0; bytes.len()];
                let emitted = parsed.emit(&mut again).expect("what parsed fits where it came from");
                assert_eq!(eth::Frame::parse(&again[..emitted]), Ok(parsed), "a frame it cannot rewrite");
            }
        }
        Frame::Arp(bytes) => {
            if let Ok(parsed) = arp::Packet::parse(bytes) {
                let mut again = [0; arp::Packet::LEN];
                let emitted = parsed.emit(&mut again).expect("a fixed-length packet fits");
                assert_eq!(arp::Packet::parse(&again[..emitted]), Ok(parsed), "a packet it cannot rewrite");
            }
        }
        Frame::Ipv4(bytes) => {
            if let Ok(parsed) = Ipv4::parse(bytes) {
                assert!(parsed.payload().len() < bytes.len(), "payload outgrew its packet");
                let mut again = vec![0; bytes.len()];
                let emitted = parsed.emit(&mut again).expect("what parsed fits where it came from");
                assert_eq!(Ipv4::parse(&again[..emitted]), Ok(parsed), "a packet it cannot rewrite");
            }
        }
        Frame::Ipv6(bytes) => {
            if let Ok(parsed) = Ipv6::parse(bytes) {
                assert!(parsed.payload().len() < bytes.len(), "payload outgrew its packet");
                let mut again = vec![0; bytes.len()];
                let emitted = parsed.emit(&mut again).expect("what parsed fits where it came from");
                assert_eq!(Ipv6::parse(&again[..emitted]), Ok(parsed), "a packet it cannot rewrite");
            }
        }
        Frame::Icmpv6 { checked, bytes } => {
            let mut message = bytes.to_vec();
            if checked && message.len() >= 8 {
                message[2..4].fill(0);
                let sum = checksum::over_ipv6(PEER_V6, LOCAL_V6, icmpv6::PROTOCOL, &message);
                message[2..4].copy_from_slice(&sum.to_be_bytes());
            }
            if let Ok(parsed) = Message::parse(PEER_V6, LOCAL_V6, &message) {
                assert!(parsed.bytes() <= message.len(), "message outgrew its packet");
                let mut again = vec![0; parsed.bytes()];
                let emitted = parsed
                    .emit(PEER_V6, LOCAL_V6, &mut again)
                    .expect("what parsed fits where it came from");
                assert_eq!(
                    Message::parse(PEER_V6, LOCAL_V6, &again[..emitted]),
                    Ok(parsed),
                    "a message it cannot rewrite"
                );
            }
        }
    }
});
