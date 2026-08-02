//! The QEMU VirtIO-net, UDP, and TCP round-trip smokes.

use molt_arch::dma::Arena;
use molt_arch::memory::{Inventory, Owner, Rights};
use molt_arch::{BootInfo, FrameAllocator, Platform, SerialWriter};
use molt_core::CellId;
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::ring::{IoRing, RequestId, Submission};
use molt_kernel::report;
use molt_net::{Config, Ip, IpAddr, IpDone, IpError, IpOp, Ipv4Addr, Ipv6Addr, Link};
use molt_pci::{Bus, Command, bus_span};
use molt_tcp::{SocketStorage, Tcp, TcpDone, TcpError, TcpOp};
use molt_udp::{Endpoint, Scratch, Udp, UdpDone, UdpError, UdpOp};
use molt_virtio::{Net, Transport};

use crate::device::{self, Line};

const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_NET: u16 = 0x1041;
const DMA_FRAMES: usize = 12;
const NET_TAG: u32 = 0x6e65_7400;
const OWNER: CellId = CellId::new(3);
const LOCAL_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GATEWAY_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const DNS_V4: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
// QEMU hands the same slirp network out on fec0::/64.
const LOCAL_V6: Ipv6Addr = Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0x15);
const GATEWAY_V6: Ipv6Addr = Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 2);
const DNS_V6: Ipv6Addr = Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 3);
const DNS_PORT: u16 = 53;
const LOCAL_PORT: u16 = 49_152;
/// A busy-wait rate measured under TCG, close enough to time a retransmission.
const SPINS_PER_MILLI: u32 = 3_000;
/// How long a round trip is given before the smoke calls it lost.
const DELIVERY_MILLIS: u32 = 5_000;
const DELIVERY_SPINS: u32 = DELIVERY_MILLIS * SPINS_PER_MILLI;
/// How often a stream is driven with nothing arriving, so its timers still run.
const IDLE_SPINS: u32 = 10 * SPINS_PER_MILLI;
// Where xtask points slirp's forwarder, which answers with what it was sent.
const ECHO: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 100);
const ECHO_PORT: u16 = 80;
const ECHO_PAYLOAD: [u8; 4] = *b"molt";

const DNS_QUERY: [u8; 29] = [
    0x4d, 0x4f, // transaction ID
    0x01, 0x00, // recursion desired
    0x00, 0x01, // one question
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // no answers, authority, or additional
    0x07, b'e', b'x', b'a', b'm', b'p', b'l', b'e', // example
    0x03, b'c', b'o', b'm', 0x00, // com
    0x00, 0x01, // A
    0x00, 0x01, // IN
];

pub fn smoke<P: Platform>(boot_info: &BootInfo<'_>, platform: &mut P) {
    let Ok(space) = platform.config_space(boot_info) else {
        return;
    };
    let (Some(cursor), Some(offset)) = (platform.free_frames(), boot_info.physical_offset()) else {
        report!(platform, "MOLT_NET_SKIPPED: this platform hands out no DMA frames");
        return;
    };
    let inventory = Inventory::new(boot_info.memory_map());
    let bus_zero = bus_span(space, space.first_bus()).expect("bus zero inside the ECAM window");
    let ecam = inventory.device(bus_zero).expect("the ECAM window is not kernel RAM");
    let window = platform.map_device(ecam, Rights::READ_WRITE).expect("a mappable ECAM window");
    let mut bus = Bus::new(&window, space.first_bus());
    let mut target = None;
    while let Some(function) = bus.function() {
        if function.vendor() == VIRTIO_VENDOR && function.device() == VIRTIO_NET {
            target = Some(function);
            break;
        }
    }
    let Some(mut function) = target else {
        report!(platform, "MOLT_NET_SKIPPED: no virtio-net device on bus zero");
        return;
    };

    let transport = Transport::probe(&function).expect("a modern network transport");
    let transport_bar = transport.common().bar();
    assert!(
        transport.notify().bar() == transport_bar && transport.device().bar() == transport_bar,
        "virtio-net structures split across BARs",
    );
    let capability = function.msix().expect("virtio-net exposes MSI-X");
    let table_bar = capability.table_bar();
    let (bar, registers) = device::map_bar(platform, &inventory, &mut function, transport_bar);
    let (table_bar, table_mapping) = if table_bar == transport_bar {
        (bar, None)
    } else {
        let (table_bar, mapping) = device::map_bar(platform, &inventory, &mut function, table_bar);
        (table_bar, Some(mapping))
    };

    let command = function.command().expect("the network command register");
    function
        .set_command(
            command.with(Command::MEMORY).with(Command::BUS_MASTER).with(Command::INTX_DISABLE),
        )
        .expect("network decode and DMA authority");

    let table = table_mapping.as_ref().unwrap_or(&registers);
    let vectored = device::route(platform, &function, capability, table, device::delta(table_bar));
    let delta = device::delta(bar);
    let common = device::subwindow(&registers, delta, transport.common());
    let notify = device::subwindow(&registers, delta, transport.notify());
    let config = device::subwindow(&registers, delta, transport.device());
    let mut allocator = FrameAllocator::resume(boot_info.memory_map(), cursor);
    let mut slots: [Option<Owner>; DMA_FRAMES] = [None; DMA_FRAMES];
    let arena = Arena::claim(&mut allocator, offset, NET_TAG, &mut slots)
        .expect("a contiguous network DMA span");
    let net = Net::start(
        common,
        notify,
        config,
        transport.notify_multiplier(),
        vectored.index(),
        device::requester(function.address()),
        arena,
    )
    .expect("the network device completes its handshake");
    let mac = net.mac();
    report!(platform, "MOLT_NET_OK: {} mac {:02x?}", function.address(), mac.octets(),);

    let config = Config::new(mac, IpAddr::V4(LOCAL_V4), 24, IpAddr::V4(GATEWAY_V4));
    let mut ip = Ip::<_, 4>::new(net, config);
    let reply = udp_round_trip(&vectored.line(), &mut ip, IpAddr::V4(DNS_V4)).expect("a DNS reply");
    report!(platform, "MOLT_UDP_OK: DNS replied with {reply} bytes");

    // The same query over v6, which cannot leave the host until an
    // advertisement answers it: nothing here knows the resolver's MAC yet.
    let config = Config::new(mac, IpAddr::V6(LOCAL_V6), 64, IpAddr::V6(GATEWAY_V6));
    let mut ip = Ip::<_, 4>::new(ip.into_link(), config);
    let _ = udp_round_trip(&vectored.line(), &mut ip, IpAddr::V6(DNS_V6));
    report!(platform, "MOLT_NDP_OK: {DNS_V6} answered a solicitation");

    let config = Config::new(mac, IpAddr::V4(LOCAL_V4), 24, IpAddr::V4(GATEWAY_V4));
    let (net, echoed) = tcp_echo(&vectored.line(), ip.into_link(), config);
    report!(platform, "MOLT_TCP_OK: {ECHO} echoed {echoed} bytes");

    net.reset().expect("the network device stops before DMA frames return");
    vectored.stop(platform);
}

/// Queries DNS and returns the reply, which only v4 gets: slirp resolves over
/// whatever nameservers the host has, and those answer on v4. A v6 query stops
/// once it is on the wire, which discovery alone decides.
fn udp_round_trip(line: &Line, ip: &mut Ip<Net<'_, '_>, 4>, dns: IpAddr) -> Option<usize> {
    let mut source = DNS_QUERY;
    let mut target = [0u8; 512];
    let mut tx = [0u8; 1480];
    let mut rx = [0u8; 1480];
    let mut buffers = BufferRegistry::<4>::new();
    let source = buffers.register_read(OWNER, &mut source).expect("one source slot");
    let target = buffers.register_write(OWNER, &mut target).expect("one receive slot");
    let tx = buffers.register_read_write(OWNER, &mut tx).expect("one TX scratch slot");
    let rx = buffers.register_read_write(OWNER, &mut rx).expect("one RX scratch slot");
    let tx = Scratch::from_registered(tx, 1480, &buffers).expect("bounded TX scratch");
    let rx = Scratch::from_registered(rx, 1480, &buffers).expect("bounded RX scratch");
    let mut ip_ring = IoRing::<IpOp, Result<IpDone, IpError>, 8>::new();
    let (mut ip_client, mut ip_driver) = ip_ring.split();
    let mut udp_ring = IoRing::<UdpOp, Result<UdpDone, UdpError>, 8>::new();
    let (mut client, mut driver) = udp_ring.split();
    let local = match dns {
        IpAddr::V4(_) => IpAddr::V4(LOCAL_V4),
        IpAddr::V6(_) => IpAddr::V6(LOCAL_V6),
    };
    let mut udp = Udp::<4, 2>::new(local, tx, rx);

    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    ip.serve(OWNER, &mut ip_driver, &mut buffers);
    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    ip.serve(OWNER, &mut ip_driver, &mut buffers);

    client
        .try_submit(Submission::new(RequestId::new(1), UdpOp::Bind { port: LOCAL_PORT }))
        .expect("room for a UDP bind");
    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    let Some(Ok(UdpDone::Bound(socket))) =
        client.try_completion().map(|completion| completion.into_result())
    else {
        panic!("UDP bind did not return a socket");
    };

    client
        .try_submit(Submission::new(
            RequestId::new(2),
            UdpOp::Recv { socket, payload: BufferOperation::new(target, 0, 512) },
        ))
        .expect("room for a UDP receive");
    client
        .try_submit(Submission::new(
            RequestId::new(3),
            UdpOp::Send {
                socket,
                to: Endpoint::new(dns, DNS_PORT),
                payload: BufferOperation::new(source, 0, DNS_QUERY.len()),
            },
        ))
        .expect("room for a UDP send");
    udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);
    ip.serve(OWNER, &mut ip_driver, &mut buffers);

    let mut frame = [0u8; 1514];
    let mut spins = 0;
    loop {
        if line.arrivals() == 0 {
            spins += 1;
            assert!(spins < DELIVERY_SPINS, "the DNS datagram received no device interrupt");
            core::hint::spin_loop();
            continue;
        }
        loop {
            let received =
                ip.link_mut().receive(&mut frame).expect("an interrupt-delivered RX frame");
            let Some(len) = received else { break };
            let _ = ip.ingest(&frame[..len], &mut ip_driver, &mut buffers);
        }
        ip.serve(OWNER, &mut ip_driver, &mut buffers);
        udp.serve(OWNER, &mut driver, &mut ip_client, &mut buffers);

        while let Some(completion) = client.try_completion() {
            match (completion.id(), completion.into_result()) {
                (id, Ok(UdpDone::Received { from, len })) if id == RequestId::new(2) => {
                    assert_eq!(from, Endpoint::new(dns, DNS_PORT));
                    let bytes = buffers
                        .resolve_write(BufferOperation::new(target, 0, len))
                        .expect("the registered DNS reply");
                    assert!(len >= 12, "DNS returned a truncated header");
                    assert_eq!(&bytes[..2], &DNS_QUERY[..2], "DNS transaction ID changed");
                    assert_ne!(bytes[2] & 0x80, 0, "DNS reply bit was clear");
                    return Some(len);
                }
                (id, Ok(UdpDone::Sent(len))) if id == RequestId::new(3) => {
                    assert_eq!(len, DNS_QUERY.len());
                    if matches!(dns, IpAddr::V6(_)) {
                        return None;
                    }
                }
                (_, result) => panic!("unexpected UDP completion: {result:?}"),
            }
        }
    }
}

/// Opens a stream to slirp's forwarder, writes to it, and reads the echo back.
///
/// Milliseconds are counted in spins: nothing here has a free-running timer
/// yet, and a retransmission timer only needs a clock tracking real time
/// closely enough to fire once. Idle polls keep it moving.
fn tcp_echo<L: Link>(line: &Line, link: L, config: Config) -> (L, usize) {
    let mut source = ECHO_PAYLOAD;
    let mut target = [0u8; 16];
    let mut slots = [SocketStorage::EMPTY; 1];
    let mut rings = [0u8; 4096];
    let mut buffers = BufferRegistry::<2>::new();
    let source = buffers.register_read(OWNER, &mut source).expect("one source slot");
    let target = buffers.register_write(OWNER, &mut target).expect("one receive slot");
    let mut tcp = Tcp::<_, 1>::new(link, config, &mut slots, &mut rings).expect("one TCP stream");
    let mut ring = IoRing::<TcpOp, Result<TcpDone, TcpError>, 4>::new();
    let (mut client, mut driver) = ring.split();

    let to = Endpoint::new(IpAddr::V4(ECHO), ECHO_PORT);
    client
        .try_submit(Submission::new(RequestId::new(1), TcpOp::Connect { to }))
        .expect("room for a TCP connect");

    let mut stream = None;
    let (mut seen, mut spins, mut submitted) = (line.arrivals(), 0u32, true);
    loop {
        spins += 1;
        assert!(spins < DELIVERY_SPINS, "the echo stream never came back");
        let arrivals = line.arrivals();
        if !submitted && arrivals == seen && spins % IDLE_SPINS != 0 {
            core::hint::spin_loop();
            continue;
        }
        (seen, submitted) = (arrivals, false);
        tcp.serve(OWNER, (spins / SPINS_PER_MILLI) as u64, &mut driver, &mut buffers);

        while let Some(completion) = client.try_completion() {
            match completion.into_result() {
                // A stream serves one request at a time, so the read is only
                // submitted once the write it answers has left.
                Ok(TcpDone::Opened(socket)) => {
                    let payload = BufferOperation::new(source, 0, ECHO_PAYLOAD.len());
                    client
                        .try_submit(Submission::new(
                            RequestId::new(2),
                            TcpOp::Send { socket, payload },
                        ))
                        .expect("room for a TCP send");
                    stream = Some(socket);
                    submitted = true;
                }
                Ok(TcpDone::Sent(len)) => {
                    assert_eq!(len, ECHO_PAYLOAD.len());
                    let socket = stream.expect("a stream the send went out on");
                    client
                        .try_submit(Submission::new(
                            RequestId::new(3),
                            TcpOp::Recv { socket, payload: BufferOperation::new(target, 0, 16) },
                        ))
                        .expect("room for a TCP receive");
                    submitted = true;
                }
                Ok(TcpDone::Received(len)) => {
                    let bytes = buffers
                        .resolve_write(BufferOperation::new(target, 0, len))
                        .expect("the registered echo");
                    assert_eq!(bytes, &ECHO_PAYLOAD, "the forwarder changed the payload");
                    return (tcp.into_link(), len);
                }
                result => panic!("unexpected TCP completion: {result:?}"),
            }
        }
    }
}
