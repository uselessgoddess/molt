//! The QEMU VirtIO-net and UDP round-trip smoke.

use molt_arch::dma::Arena;
use molt_arch::memory::{Inventory, Owner, Rights};
use molt_arch::{BootInfo, FrameAllocator, Platform, SerialWriter};
use molt_core::buffer::{BufferOperation, BufferRegistry};
use molt_core::capability::CellId;
use molt_core::ring::{IoRing, RequestId, Submission};
use molt_kernel::report;
use molt_net::addr::{IpAddr, Ipv4Addr};
use molt_net::{Config, Ip, IpDone, IpError, IpOp};
use molt_pci::{Bus, Command, bus_span};
use molt_udp::{Endpoint, Scratch, Udp, UdpDone, UdpError, UdpOp};
use molt_virtio::{Net, Transport};

use crate::device::{self, Line};

const VIRTIO_VENDOR: u16 = 0x1af4;
const VIRTIO_NET: u16 = 0x1041;
const DMA_FRAMES: usize = 10;
const NET_TAG: u32 = 0x6e65_7400;
const OWNER: CellId = CellId::new(3);
const LOCAL: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const DNS: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
const DNS_PORT: u16 = 53;
const LOCAL_PORT: u16 = 49_152;
const DELIVERY_SPINS: u32 = 50_000_000;

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
        .expect("ten contiguous network DMA frames");
    let net =
        Net::start(common, notify, config, transport.notify_multiplier(), vectored.index(), arena)
            .expect("the network device completes its handshake");
    let mac = net.mac();
    report!(platform, "MOLT_NET_OK: {} mac {:02x?}", function.address(), mac.octets(),);

    let mut ip = Ip::<_, 4>::new(net, Config::new(mac, IpAddr::V4(LOCAL), 24, IpAddr::V4(GATEWAY)));
    let reply = udp_round_trip(&vectored.line(), &mut ip);
    report!(platform, "MOLT_UDP_OK: DNS replied with {reply} bytes");

    let net = ip.into_link();
    net.reset().expect("the network device stops before DMA frames return");
    vectored.stop(platform);
}

fn udp_round_trip(line: &Line, ip: &mut Ip<Net<'_, '_>, 4>) -> usize {
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
    let mut udp = Udp::<4, 2>::new(IpAddr::V4(LOCAL), tx, rx);

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
                to: Endpoint::new(IpAddr::V4(DNS), DNS_PORT),
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
                    assert_eq!(from, Endpoint::new(IpAddr::V4(DNS), DNS_PORT));
                    let bytes = buffers
                        .resolve_write(BufferOperation::new(target, 0, len))
                        .expect("the registered DNS reply");
                    assert!(len >= 12, "DNS returned a truncated header");
                    assert_eq!(&bytes[..2], &DNS_QUERY[..2], "DNS transaction ID changed");
                    assert_ne!(bytes[2] & 0x80, 0, "DNS reply bit was clear");
                    return len;
                }
                (id, Ok(UdpDone::Sent(len))) if id == RequestId::new(3) => {
                    assert_eq!(len, DNS_QUERY.len());
                }
                (_, result) => panic!("unexpected UDP completion: {result:?}"),
            }
        }
    }
}
