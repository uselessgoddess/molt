//! An interrupt-driven VirtIO network link.
//!
//! Initialization fills the receive queue before setting `DRIVER_OK`. After
//! the platform routes its MSI-X entry, an interrupt consumer drains
//! [`receive`](Net::receive); each used buffer is republished before its frame
//! is returned. No receive path polls the device.
//!
//! Both directions stage a queue's worth of frames, so a sender does not wait
//! on the device to retire the frame before it.

use molt_arch::Mmio;
use molt_arch::dma::Arena;
use molt_arch::iommu::{DeviceId, DmaPerm, Mapper, Mapping};
use molt_net::addr::MacAddr;
use molt_net::{Link, LinkError};

use crate::VirtioError;
use crate::config::{Common, status};
use crate::notify::Notify;
use crate::queue::{self, MAX_SIZE, Queue};

const RECEIVE_QUEUE: u16 = 0;
const TRANSMIT_QUEUE: u16 = 1;

/// Device configuration contains a stable MAC address.
const VIRTIO_NET_F_MAC: u64 = 1 << 5;

/// The device uses the complete modern header, including `num_buffers`.
const VIRTIO_NET_F_MRG_RXBUF: u64 = 1 << 15;

const REQUIRED_FEATURES: u64 = VIRTIO_NET_F_MAC | VIRTIO_NET_F_MRG_RXBUF;

/// The modern VirtIO network header includes `num_buffers`.
const HEADER: usize = 12;
const FRAME: usize = molt_net::FRAME;
/// One staged frame in either direction: a header and the frame behind it.
const BUFFER: usize = HEADER + FRAME;
const CONFIG_SPINS: u32 = 16;
const NETWORK_SIZE: u16 = 8;

struct Receive {
    queue: Queue,
    buffers: Mapping,
    heads: [Option<u16>; MAX_SIZE as usize],
    notify_off: u16,
}

impl Receive {
    fn new(queue: Queue, buffers: Mapping, notify_off: u16) -> Result<Self, VirtioError> {
        if buffers.len() < queue.size() as u64 * BUFFER as u64 {
            return Err(VirtioError::Device);
        }
        let mut receive = Self { queue, buffers, heads: [None; MAX_SIZE as usize], notify_off };
        for slot in 0..receive.queue.size() {
            receive.post(slot)?;
        }
        Ok(receive)
    }

    fn post(&mut self, slot: u16) -> Result<(), VirtioError> {
        let offset = slot as u64 * BUFFER as u64;
        let segment = self.buffers.writable(offset, BUFFER as u32)?;
        let head = self.queue.push(&[segment])?;
        let mapped = self.heads.get_mut(head as usize).ok_or(VirtioError::Device)?;
        if mapped.replace(slot).is_some() {
            return Err(VirtioError::Device);
        }
        Ok(())
    }

    fn receive(&mut self, frame: &mut [u8]) -> Result<Option<usize>, VirtioError> {
        let Some(used) = self.queue.pop()? else {
            return Ok(None);
        };
        let slot = self
            .heads
            .get_mut(used.head() as usize)
            .and_then(Option::take)
            .ok_or(VirtioError::Device)?;
        let written = used.written() as usize;
        let result = (|| {
            if !(HEADER..=BUFFER).contains(&written) {
                return Err(VirtioError::Device);
            }
            let len = written - HEADER;
            if len > frame.len() {
                return Err(VirtioError::Full);
            }
            let offset = slot as u64 * BUFFER as u64;
            self.validate_header(offset)?;
            self.buffers.read_into(offset + HEADER as u64, &mut frame[..len])?;
            Ok(Some(len))
        })();

        // A malformed packet must not drain the receive queue either.
        self.post(slot)?;
        result
    }

    fn validate_header(&self, offset: u64) -> Result<(), VirtioError> {
        let flags = self.buffers.read_u8(offset)?;
        let gso = self.buffers.read_u8(offset + 1)?;
        let buffers = self.buffers.read_u16(offset + 10)?;
        if flags != 0 || gso != 0 || buffers != 1 {
            return Err(VirtioError::Device);
        }
        Ok(())
    }

    fn mappings(self) -> impl Iterator<Item = Mapping> {
        self.queue.mappings().into_iter().chain([self.buffers])
    }

    fn kick(&self, notify: &Notify<'_>) -> Result<(), VirtioError> {
        notify.signal(RECEIVE_QUEUE, self.notify_off)
    }
}

struct Transmit {
    queue: Queue,
    buffers: Mapping,
    heads: [Option<u16>; MAX_SIZE as usize],
    notify_off: u16,
    /// The staging slots no frame is in flight from, one bit each.
    free: u16,
}

impl Transmit {
    fn new(queue: Queue, buffers: Mapping, notify_off: u16) -> Result<Self, VirtioError> {
        if buffers.len() < queue.size() as u64 * BUFFER as u64 {
            return Err(VirtioError::Device);
        }
        let free = (1 << queue.size()) - 1;
        Ok(Self { queue, buffers, heads: [None; MAX_SIZE as usize], notify_off, free })
    }

    /// Copies `frame` into a free slot and hands the device the slot.
    fn stage(&mut self, frame: &[u8]) -> Result<(), VirtioError> {
        self.reap()?;
        let slot = self.free.trailing_zeros() as u16;
        if slot >= self.queue.size() {
            return Err(VirtioError::Full);
        }
        let offset = slot as u64 * BUFFER as u64;
        self.buffers.write_from(offset, &[0; HEADER])?;
        self.buffers.write_from(offset + HEADER as u64, frame)?;
        let len = u32::try_from(HEADER + frame.len()).map_err(|_| VirtioError::Device)?;
        let segment = self.buffers.readable(offset, len)?;
        let head = self.queue.push(&[segment])?;
        let mapped = self.heads.get_mut(head as usize).ok_or(VirtioError::Device)?;
        if mapped.replace(slot).is_some() {
            return Err(VirtioError::Device);
        }
        self.free &= !(1 << slot);
        Ok(())
    }

    /// Frees every slot the device has finished reading.
    fn reap(&mut self) -> Result<(), VirtioError> {
        while let Some(used) = self.queue.pop()? {
            let slot = self
                .heads
                .get_mut(used.head() as usize)
                .and_then(Option::take)
                .ok_or(VirtioError::Device)?;
            self.free |= 1 << slot;
        }
        Ok(())
    }

    fn mappings(self) -> impl Iterator<Item = Mapping> {
        self.queue.mappings().into_iter().chain([self.buffers])
    }

    fn kick(&self, notify: &Notify<'_>) -> Result<(), VirtioError> {
        notify.signal(TRANSMIT_QUEUE, self.notify_off)
    }
}

pub struct Nic<'w> {
    common: Common<'w>,
    notify: Notify<'w>,
    mac: MacAddr,
}

/// A VirtIO network device with preposted RX buffers and a staged TX ring.
pub struct Net<'s, 'w, M: Mapper> {
    mapper: M,
    nic: Nic<'w>,
    receive: Receive,
    transmit: Transmit,
    arena: Arena<'s>,
}

impl<'s, 'w, M: Mapper> Net<'s, 'w, M> {
    /// Brings up queue pair zero and routes both queues through `vector`.
    #[allow(clippy::too_many_arguments)] // FIXME: split into high-level types or reorganize 
    pub fn start(
        common: Mmio<'w>,
        notify: Mmio<'w>,
        device: Mmio<'w>,
        notify_multiplier: u32,
        vector: u16,
        endpoint: DeviceId,
        mut mapper: M,
        mut arena: Arena<'s>,
    ) -> Result<Self, VirtioError> {
        let mut common = Common::new(common);
        common.reset()?;
        common.add_status(status::ACKNOWLEDGE)?;
        common.add_status(status::DRIVER)?;
        let features = common.negotiate(REQUIRED_FEATURES)?;
        require_features(features)?;
        if common.num_queues()? < 2 {
            return Err(VirtioError::Missing);
        }
        let mac = mac(&common, &device)?;

        let (receive_queue, receive_notify) =
            queue(&mut common, &mut arena, &mut mapper, endpoint, RECEIVE_QUEUE, vector)?;
        let receive_buffers = map(
            &mut mapper,
            endpoint,
            arena.region(receive_queue.size() as u64 * BUFFER as u64)?,
            DmaPerm::WRITE,
        )?;
        let receive = Receive::new(receive_queue, receive_buffers, receive_notify)?;

        let (transmit_queue, transmit_notify) =
            queue(&mut common, &mut arena, &mut mapper, endpoint, TRANSMIT_QUEUE, vector)?;
        let transmit_buffers = map(
            &mut mapper,
            endpoint,
            arena.region(transmit_queue.size() as u64 * BUFFER as u64)?,
            DmaPerm::READ,
        )?;
        let transmit = Transmit::new(transmit_queue, transmit_buffers, transmit_notify)?;

        // RX descriptors are visible before the device begins running.
        common.add_status(status::DRIVER_OK)?;
        let notify = Notify::new(notify, notify_multiplier);
        notify.signal(RECEIVE_QUEUE, receive_notify)?;

        let nic = Nic { common, notify, mac };
        Ok(Self { nic, receive, transmit, mapper, arena })
    }

    /// The device-provided Ethernet address negotiated at startup.
    pub const fn mac(&self) -> MacAddr {
        self.nic.mac
    }

    /// Drains one interrupt-reported RX completion and immediately reposts it.
    pub fn receive(&mut self, frame: &mut [u8]) -> Result<Option<usize>, VirtioError> {
        let received = self.receive.receive(frame);
        self.receive.kick(&self.nic.notify)?;
        received
    }

    /// Stops DMA before returning every queue and buffer frame to its arena.
    pub fn reset(self) -> Result<(), VirtioError> {
        let Self { mut nic, receive, transmit, mut mapper, mut arena, .. } = self;
        nic.common.reset()?;
        let released = receive
            .mappings()
            .chain(transmit.mappings())
            .map(|mapping| mapper.unmap(mapping).map_err(|error| error.error()))
            .try_for_each(|region| arena.release(region?));
        arena.reset();
        Ok(released?)
    }
}

impl<M: Mapper> Link for Net<'_, '_, M> {
    fn transmit(&mut self, frame: &[u8]) -> Result<(), LinkError> {
        if frame.len() > FRAME {
            return Err(LinkError::Device);
        }
        self.transmit.stage(frame).map_err(|error| match error {
            VirtioError::Full => LinkError::Busy,
            _ => LinkError::Device,
        })?;
        self.transmit.kick(&self.nic.notify).map_err(|_| LinkError::Device)
    }

    fn receive(&mut self, frame: &mut [u8]) -> Result<Option<usize>, LinkError> {
        Net::receive(self, frame).map_err(|_| LinkError::Device)
    }
}

fn queue<M: Mapper>(
    common: &mut Common<'_>,
    arena: &mut Arena<'_>,
    mapper: &mut M,
    endpoint: DeviceId,
    index: u16,
    vector: u16,
) -> Result<(Queue, u16), VirtioError> {
    common.select_queue(index)?;
    let size = clamp_queue(common.queue_size()?)?;
    let descriptors =
        map(mapper, endpoint, arena.region(queue::descriptor_bytes(size))?, DmaPerm::READ)?;
    let driver = map(mapper, endpoint, arena.region(queue::driver_bytes(size))?, DmaPerm::READ)?;
    let device =
        map(mapper, endpoint, arena.region(queue::device_bytes(size))?, DmaPerm::READ_WRITE)?;
    let queue = Queue::new(size, descriptors, driver, device)?;
    common.set_queue_size(size)?;
    common.set_queue_vector(vector)?;
    common.set_queue_rings(queue.descriptors_iova(), queue.driver_iova(), queue.device_iova())?;
    common.enable_queue()?;
    Ok((queue, common.queue_notify_off()?))
}

fn map(
    mapper: &mut impl Mapper,
    endpoint: DeviceId,
    region: molt_arch::dma::Region,
    perm: DmaPerm,
) -> Result<Mapping, VirtioError> {
    mapper.map(endpoint, region, perm).map_err(|error| VirtioError::Dma(error.error()))
}

fn mac(common: &Common<'_>, device: &Mmio<'_>) -> Result<MacAddr, VirtioError> {
    for _ in 0..CONFIG_SPINS {
        let before = common.config_generation()?;
        let mut octets = [0u8; 6];
        for (offset, octet) in octets.iter_mut().enumerate() {
            *octet = device.read_u8(offset as u64)?;
        }
        if common.config_generation()? == before {
            return Ok(MacAddr::new(octets));
        }
    }
    Err(VirtioError::Device)
}

fn clamp_queue(device_max: u16) -> Result<u16, VirtioError> {
    if device_max == 0 {
        return Err(VirtioError::Device);
    }
    let size = device_max.min(NETWORK_SIZE);
    if !size.is_power_of_two() {
        return Err(VirtioError::Device);
    }
    Ok(size)
}

fn require_features(features: u64) -> Result<(), VirtioError> {
    if features & REQUIRED_FEATURES != REQUIRED_FEATURES {
        return Err(VirtioError::Features);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use molt_arch::dma::Region;
    use molt_arch::iommu::{DeviceId, DmaPerm, Identity, Mapper, Mapping};

    use super::{
        BUFFER, HEADER, NETWORK_SIZE, REQUIRED_FEATURES, Receive, Transmit, VIRTIO_NET_F_MAC,
        clamp_queue, require_features,
    };
    use crate::VirtioError;
    use crate::queue::{Queue, device_bytes, driver_bytes};

    fn region(bytes: &mut [u8], physical: u64) -> Region {
        // SAFETY: each test array is live, uniquely borrowed, and disjoint.
        unsafe { Region::new(bytes.as_mut_ptr(), physical, bytes.len() as u64) }
    }

    fn mapping(bytes: &mut [u8], physical: u64, perm: DmaPerm) -> Mapping {
        Identity.map(DeviceId::new(0), region(bytes, physical), perm).ok().unwrap()
    }

    fn queue(descriptors: &mut [u8; 128], driver: &mut [u8; 32], device: &mut [u8; 72]) -> Queue {
        Queue::new(
            8,
            mapping(descriptors, 0x1000, DmaPerm::READ),
            mapping(&mut driver[..driver_bytes(8) as usize], 0x2000, DmaPerm::READ),
            mapping(&mut device[..device_bytes(8) as usize], 0x3000, DmaPerm::READ_WRITE),
        )
        .unwrap()
    }

    #[test]
    fn receive_reposts_completed_buffer() -> Result<(), VirtioError> {
        let mut descriptors = [0u8; 128];
        let mut driver = [0u8; 32];
        let mut device = [0u8; 72];
        let queue = queue(&mut descriptors, &mut driver, &mut device);
        let mut storage = [0u8; BUFFER * 8];
        let buffers = mapping(&mut storage, 0x4000, DmaPerm::WRITE);
        let mut receive = Receive::new(queue, buffers, 0)?;
        storage[10..12].copy_from_slice(&1u16.to_le_bytes());
        storage[HEADER..HEADER + 4].copy_from_slice(b"ping");
        device[4..8].copy_from_slice(&0u32.to_le_bytes());
        device[8..12].copy_from_slice(&((HEADER + 4) as u32).to_le_bytes());
        device[2..4].copy_from_slice(&1u16.to_le_bytes());
        let mut frame = [0u8; 16];

        assert_eq!(receive.receive(&mut frame), Ok(Some(4)));
        assert_eq!(&frame[..4], b"ping");
        assert_eq!(&driver[2..4], &9u16.to_le_bytes(), "RX buffer was not republished");
        assert_eq!(receive.queue.available(), 0, "RX queue lost a descriptor");
        Ok(())
    }

    /// A stream writes back to back, so a frame must not wait on the one
    /// before it: staging the whole queue may not drop any of it.
    #[test]
    fn transmit_stages_whole_queue() -> Result<(), VirtioError> {
        let mut descriptors = [0u8; 128];
        let mut driver = [0u8; 32];
        let mut device = [0u8; 72];
        let queue = queue(&mut descriptors, &mut driver, &mut device);
        let mut storage = [0u8; BUFFER * 8];
        let buffers = mapping(&mut storage, 0x4000, DmaPerm::READ);
        let mut transmit = Transmit::new(queue, buffers, 0)?;

        for slot in 0..8u8 {
            transmit.stage(&[slot; 4])?;
        }
        assert_eq!(transmit.stage(b"molt"), Err(VirtioError::Full), "a ninth frame fit");
        for slot in 0..8usize {
            let at = slot * BUFFER + HEADER;
            assert_eq!(&storage[at..at + 4], &[slot as u8; 4], "slot {slot} was not staged");
        }
        Ok(())
    }

    #[test]
    fn modern_header_requires_merge_buffer_format() {
        assert_eq!(require_features(VIRTIO_NET_F_MAC), Err(VirtioError::Features));
        assert_eq!(require_features(REQUIRED_FEATURES), Ok(()));
    }

    #[test]
    fn queue_depth() {
        assert_eq!(clamp_queue(256), Ok(NETWORK_SIZE));
    }
}
