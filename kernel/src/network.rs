//! The QEMU VirtIO-net, UDP, and TCP round-trip smokes.

use molt_arch::dma::Arena;
use molt_arch::memory::{Inventory, Owner, Rights};
use molt_arch::{BootInfo, FrameAllocator, Platform, SerialWriter, iommu};
use molt_core::CellId;
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::ring::{IoRing, RequestId, Submission};
use molt_kernel::report;
use molt_net::{Config, Ip, IpAddr, IpDone, IpError, IpOp, Ipv4Addr, Ipv6Addr, Link};
use molt_pci::{Bus, Command, bus_span};
use molt_tcp::{SocketStorage, Tcp, TcpDone, TcpError, TcpOp};
use molt_udp::{Endpoint, Scratch, Udp, UdpDone, UdpError, UdpOp};
use molt_virtio::{Arrivals, Iommu, Net, NetConfig, Transport};

use crate::device::{self, Line};

const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_NET: u16 = 0x1041;
const VIRTIO_IOMMU: u16 = 0x1057;
const DMA_FRAMES: usize = 12;
const IOMMU_FRAMES: usize = 8;
const NET_TAG: u32 = 0x6e65_7400;
const IOMMU_TAG: u32 = 0x10ab;
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

struct Poll;

impl Arrivals for Poll {
    fn wait(&mut self) -> u64 {
        core::hint::spin_loop();
        0
    }
}

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
    let mut controller = None;
    while let Some(function) = bus.function() {
        if function.vendor() == VIRTIO_VENDOR {
            match function.device() {
                VIRTIO_NET => target = Some(function),
                VIRTIO_IOMMU => controller = Some(function),
                _ => {}
            }
        }
    }
    let (Some(mut function), Some(mut iommu_function)) = (target, controller) else {
        report!(platform, "MOLT_NET_SKIPPED: no virtio-net/IOMMU pair on bus zero");
        return;
    };

    let iommu_transport =
        Transport::probe(&iommu_function).expect("the network IOMMU describes its structures");
    let iommu_bar_index = iommu_transport.common().bar();
    assert!(
        iommu_transport.notify().bar() == iommu_bar_index
            && iommu_transport.device().bar() == iommu_bar_index,
        "IOMMU structures split across BARs",
    );
    let (iommu_bar, iommu_registers) =
        device::map_bar(platform, &inventory, &mut iommu_function, iommu_bar_index);
    let iommu_command = iommu_function.command().expect("the network IOMMU command register");
    iommu_function
        .set_command(
            iommu_command
                .with(Command::MEMORY)
                .with(Command::BUS_MASTER)
                .with(Command::INTX_DISABLE),
        )
        .expect("network IOMMU decode and DMA authority");
    let iommu_delta = device::delta(iommu_bar);
    let iommu_common = device::subwindow(&iommu_registers, iommu_delta, iommu_transport.common());
    let iommu_notify = device::subwindow(&iommu_registers, iommu_delta, iommu_transport.notify());
    let iommu_config = device::subwindow(&iommu_registers, iommu_delta, iommu_transport.device());

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
    let quiesced =
        command.with(Command::MEMORY).with(Command::INTX_DISABLE).without(Command::BUS_MASTER);
    function.set_command(quiesced).expect("network stays quiesced before mappings exist");

    let table = table_mapping.as_ref().unwrap_or(&registers);
    let vectored = device::route(platform, &function, capability, table, device::delta(table_bar));
    let delta = device::delta(bar);
    let common = device::subwindow(&registers, delta, transport.common());
    let notify = device::subwindow(&registers, delta, transport.notify());
    let config = device::subwindow(&registers, delta, transport.device());
    let mut allocator = FrameAllocator::resume(boot_info.memory_map(), cursor);
    let mut iommu_slots: [Option<Owner>; IOMMU_FRAMES] = [None; IOMMU_FRAMES];
    let iommu_arena = Arena::claim(&mut allocator, offset, IOMMU_TAG, &mut iommu_slots)
        .expect("contiguous frames for network IOMMU queues");
    let mut slots: [Option<Owner>; DMA_FRAMES] = [None; DMA_FRAMES];
    let arena = Arena::claim(&mut allocator, offset, NET_TAG, &mut slots)
        .expect("a contiguous network DMA span");
    let endpoint = device::requester(function.address());
    let mut iommu = Iommu::start(
        iommu_common,
        iommu_notify,
        iommu_config,
        iommu_transport.notify_multiplier(),
        u16::MAX,
        Poll,
        device::requester(iommu_function.address()),
        iommu_arena,
    )
    .expect("the network IOMMU completes its handshake");
    iommu.attach(endpoint).expect("network endpoint attaches while quiesced");
    let net = Net::start(
        NetConfig::new(
            common,
            notify,
            config,
            transport.notify_multiplier(),
            vectored.index(),
            endpoint,
        ),
        iommu,
        arena,
    )
    .expect("the network device completes its handshake");
    function
        .set_command(quiesced.with(Command::BUS_MASTER))
        .expect("network bus mastering follows mappings");
    report!(platform, "MOLT_NET_IOMMU_OK: endpoint mapped before bus mastering");
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

    let mut iommu = net.reset().expect("the network device stops before DMA frames return");
    function.set_command(quiesced).expect("network bus mastering stays off after reset");
    assert!(iommu.poll_faults().expect("the network fault queue remains valid").is_none());
    iommu.detach(endpoint).expect("the empty network domain detaches");
    iommu.reset().expect("the network IOMMU stops after its endpoint");
    iommu_function
        .set_command(
            iommu_command
                .with(Command::MEMORY)
                .with(Command::INTX_DISABLE)
                .without(Command::BUS_MASTER),
        )
        .expect("network IOMMU bus mastering stays off after reset");
    vectored.stop(platform);
}

/// Queries DNS and returns the reply, which only v4 gets: slirp resolves over
/// whatever nameservers the host has, and those answer on v4. A v6 query stops
/// once it is on the wire, which discovery alone decides.
fn udp_round_trip<M: iommu::Mapper>(
    line: &Line,
    ip: &mut Ip<Net<'_, '_, M>, 4>,
    dns: IpAddr,
) -> Option<usize> {
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
