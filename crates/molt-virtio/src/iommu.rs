//! A translating VirtIO IOMMU backend for device-scoped DMA mappings.
//!
//! The IOMMU's own two queues use identity DMA: they are the control plane
//! that creates translated address spaces for other endpoints. An endpoint is
//! attached to an isolated domain before any mapping is installed. Each map
//! consumes a [`Region`], reserves a page-aligned IOVA, and returns a
//! [`Mapping`] that is the only safe source of descriptor addresses. Unmap
//! consumes that mapping before returning its region.

use molt_arch::dma::{Arena, DmaError, Region};
use molt_arch::iommu::{
    DeviceId, DmaPerm, Identity, Iova, IovaSpace, MapError, Mapper, Mapping, UnmapError,
};
use molt_arch::{Mmio, align_up};

use crate::VirtioError;
use crate::config::{Common, status};
use crate::interrupt::Arrivals;
use crate::notify::Notify;
use crate::queue::{self, Queue};

const VIRTIO_IOMMU_F_INPUT_RANGE: u64 = 1 << 0;
const VIRTIO_IOMMU_F_DOMAIN_RANGE: u64 = 1 << 1;
const VIRTIO_IOMMU_F_MAP_UNMAP: u64 = 1 << 2;
const VIRTIO_IOMMU_F_PROBE: u64 = 1 << 4;
const VIRTIO_IOMMU_F_MMIO: u64 = 1 << 5;
const VIRTIO_IOMMU_F_BYPASS_CONFIG: u64 = 1 << 6;

const REQUEST_QUEUE: u16 = 0;
const EVENT_QUEUE: u16 = 1;
const CONFIG_SPINS: u32 = 16;

const ATTACH: u8 = 1;
const DETACH: u8 = 2;
const MAP: u8 = 3;
const UNMAP: u8 = 4;
const STATUS_OK: u8 = 0;
const STATUS_AT_ATTACH: u64 = 20;
const STATUS_AT_MAP: u64 = 36;
const STATUS_AT_UNMAP: u64 = 28;
const MAP_READ: u32 = 1;
const MAP_WRITE: u32 = 2;

const REQUEST_BYTES: u64 = 40;
const EVENT_BYTES: u64 = 24;
const ACTIVE: usize = 32;
const ENDPOINTS: usize = 8;
const PREFERRED_IOVA: u64 = 1 << 32;

#[derive(Clone, Copy)]
struct Config {
    page_size_mask: u64,
    input_start: u64,
    input_end: u64,
    domain_start: u32,
    domain_end: u32,
}

#[derive(Clone, Copy)]
struct Active {
    device: DeviceId,
    domain: u32,
    iova: Iova,
    len: u64,
    perm: DmaPerm,
}

#[derive(Clone, Copy)]
struct Attachment {
    device: DeviceId,
    domain: u32,
}

struct Domains<const N: usize> {
    start: u32,
    end: u32,
    entries: [Option<Attachment>; N],
}

impl<const N: usize> Domains<N> {
    const fn new(start: u32, end: u32) -> Self {
        Self { start, end, entries: [None; N] }
    }

    fn reserve(&mut self, device: DeviceId) -> Result<u32, VirtioError> {
        if self.domain(device).is_some() {
            return Err(VirtioError::Device);
        }
        let slot = self.entries.iter().position(Option::is_none);
        let Some(slot) = slot else {
            return Err(VirtioError::Full);
        };
        let mut domain = self.start;
        while domain <= self.end {
            if !self.entries.iter().flatten().any(|entry| entry.domain == domain) {
                self.entries[slot] = Some(Attachment { device, domain });
                return Ok(domain);
            }
            domain = domain.checked_add(1).ok_or(VirtioError::Device)?;
        }
        Err(VirtioError::Full)
    }

    fn domain(&self, device: DeviceId) -> Option<u32> {
        self.entries.iter().flatten().find(|entry| entry.device == device).map(|entry| entry.domain)
    }

    fn release(&mut self, device: DeviceId) -> Result<u32, VirtioError> {
        let slot = self
            .entries
            .iter_mut()
            .find(|entry| entry.is_some_and(|entry| entry.device == device))
            .ok_or(VirtioError::Device)?;
        let domain = slot.take().unwrap().domain;
        Ok(domain)
    }

    fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }
}

/// One asynchronous translation fault reported by the IOMMU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fault {
    reason: u8,
    flags: u32,
    endpoint: DeviceId,
    address: u64,
}

impl Fault {
    pub const fn reason(self) -> u8 {
        self.reason
    }

    pub const fn flags(self) -> u32 {
        self.flags
    }

    pub const fn endpoint(self) -> DeviceId {
        self.endpoint
    }

    pub const fn address(self) -> u64 {
        self.address
    }
}

/// A VirtIO IOMMU controlling several isolated endpoint domains.
pub struct Iommu<'slots, 'window, A> {
    common: Common<'window>,
    notify: Notify<'window>,
    request_queue: Queue,
    event_queue: Queue,
    request: Mapping,
    events: Mapping,
    event_slots: [Option<u16>; queue::MAX_SIZE as usize],
    arrivals: A,
    arena: Arena<'slots>,
    space: IovaSpace<ACTIVE>,
    active: [Option<Active>; ACTIVE],
    request_notify: u16,
    event_notify: u16,
    page: u64,
    domains: Domains<ENDPOINTS>,
    last_fault: Option<Fault>,
}

impl<'slots, 'window, A: Arrivals> Iommu<'slots, 'window, A> {
    /// Starts the IOMMU control plane. Its own queues remain identity-mapped;
    /// endpoints attached later use translated addresses.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        common: Mmio<'window>,
        notify: Mmio<'window>,
        device: Mmio<'window>,
        notify_multiplier: u32,
        vector: u16,
        arrivals: A,
        controller: DeviceId,
        mut arena: Arena<'slots>,
    ) -> Result<Self, VirtioError> {
        let mut common = Common::new(common);
        common.reset()?;
        common.add_status(status::ACKNOWLEDGE)?;
        common.add_status(status::DRIVER)?;
        let wanted = VIRTIO_IOMMU_F_INPUT_RANGE
            | VIRTIO_IOMMU_F_DOMAIN_RANGE
            | VIRTIO_IOMMU_F_MAP_UNMAP
            | VIRTIO_IOMMU_F_PROBE
            | VIRTIO_IOMMU_F_MMIO
            | VIRTIO_IOMMU_F_BYPASS_CONFIG;
        let features = common.negotiate(wanted)?;
        let required =
            VIRTIO_IOMMU_F_INPUT_RANGE | VIRTIO_IOMMU_F_DOMAIN_RANGE | VIRTIO_IOMMU_F_MAP_UNMAP;
        if features & required != required {
            return Err(VirtioError::Features);
        }
        let config = read_config(&common, &device)?;
        let page = page_size(config.page_size_mask)?;
        let (space_start, space_end) = aperture(config.input_start, config.input_end, page)?;
        let domain_start = config.domain_start.max(1);
        if domain_start > config.domain_end {
            return Err(VirtioError::Device);
        }

        let (request_queue, request_notify) =
            make_queue(&mut common, REQUEST_QUEUE, vector, controller, &mut arena, 2)?;
        let (mut event_queue, event_notify) =
            make_queue(&mut common, EVENT_QUEUE, vector, controller, &mut arena, 1)?;
        let mut identity = Identity;
        let request = identity_map(
            &mut identity,
            controller,
            arena.region(REQUEST_BYTES)?,
            DmaPerm::READ_WRITE,
        )?;
        let events = identity_map(
            &mut identity,
            controller,
            arena.region(event_queue.size() as u64 * EVENT_BYTES)?,
            DmaPerm::WRITE,
        )?;
        let mut event_slots = [None; queue::MAX_SIZE as usize];
        for slot in 0..event_queue.size() {
            let buffer = events.writable(slot as u64 * EVENT_BYTES, EVENT_BYTES as u32)?;
            let head = event_queue.push(&[buffer])?;
            event_slots[head as usize] = Some(slot);
        }

        validate_notify(&notify, notify_multiplier, request_notify)?;
        validate_notify(&notify, notify_multiplier, event_notify)?;
        common.add_status(status::DRIVER_OK)?;
        let notify = Notify::new(notify, notify_multiplier);
        notify.signal(EVENT_QUEUE, event_notify)?;

        Ok(Self {
            common,
            notify,
            request_queue,
            event_queue,
            request,
            events,
            event_slots,
            arrivals,
            arena,
            space: IovaSpace::new(Iova::new(space_start), Iova::new(space_end)),
            active: [None; ACTIVE],
            request_notify,
            event_notify,
            page,
            domains: Domains::new(domain_start, config.domain_end),
            last_fault: None,
        })
    }

    /// Attaches an endpoint to the isolated translation domain.
    pub fn attach(&mut self, endpoint: DeviceId) -> Result<(), VirtioError> {
        let domain = self.domains.reserve(endpoint)?;
        let attached = encode_attach(&self.request, domain, endpoint)
            .and_then(|()| self.command(STATUS_AT_ATTACH));
        if attached.is_err() {
            self.domains.release(endpoint).ok();
        }
        attached
    }

    /// Detaches the endpoint once all of its mappings are gone.
    pub fn detach(&mut self, endpoint: DeviceId) -> Result<(), VirtioError> {
        if self.active.iter().flatten().any(|entry| entry.device == endpoint) {
            return Err(VirtioError::Device);
        }
        let domain = self.domains.domain(endpoint).ok_or(VirtioError::Device)?;
        encode_detach(&self.request, domain, endpoint)?;
        self.command(STATUS_AT_ATTACH)?;
        self.domains.release(endpoint)?;
        Ok(())
    }

    /// Most recently observed asynchronous fault, if any.
    pub const fn last_fault(&self) -> Option<Fault> {
        self.last_fault
    }

    /// Number of device mappings currently installed in the domain.
    pub fn mapped(&self) -> usize {
        self.active.iter().filter(|entry| entry.is_some()).count()
    }

    /// Number of mappings installed for `endpoint`.
    pub fn mapped_for(&self, endpoint: DeviceId) -> usize {
        self.active.iter().flatten().filter(|entry| entry.device == endpoint).count()
    }

    /// Polls and replenishes the fault queue.
    pub fn poll_faults(&mut self) -> Result<Option<Fault>, VirtioError> {
        let mut newest = None;
        while let Some(used) = self.event_queue.pop()? {
            let slot = self.event_slots[used.head() as usize].take().ok_or(VirtioError::Device)?;
            if used.written() < EVENT_BYTES as u32 {
                return Err(VirtioError::Device);
            }
            let fault = parse_fault(&self.events, slot as u64 * EVENT_BYTES)?;
            self.last_fault = Some(fault);
            newest = Some(fault);

            let buffer = self.events.writable(slot as u64 * EVENT_BYTES, EVENT_BYTES as u32)?;
            let head = self.event_queue.push(&[buffer])?;
            self.event_slots[head as usize] = Some(slot);
            self.notify.signal(EVENT_QUEUE, self.event_notify)?;
        }
        Ok(newest)
    }

    /// Resets the control device and gives its identity-mapped frames back.
    pub fn reset(self) -> Result<(), VirtioError> {
        if !self.domains.is_empty() || self.active.iter().any(Option::is_some) {
            return Err(VirtioError::Device);
        }
        let Self { mut common, request_queue, event_queue, request, events, mut arena, .. } = self;
        common.reset()?;
        let mut identity = Identity;
        for mapping in request_queue
            .mappings()
            .into_iter()
            .chain(event_queue.mappings())
            .chain([request, events])
        {
            let region =
                identity.unmap(mapping).map_err(|error| VirtioError::Dma(error.error()))?;
            arena.release(region)?;
        }
        arena.reset();
        Ok(())
    }

    fn command(&mut self, status_at: u64) -> Result<(), VirtioError> {
        self.request.write_u8(status_at, 0xff)?;
        let request = self.request.readable(0, status_at as u32)?;
        let response = self.request.writable(status_at, 4)?;
        let head = self.request_queue.push(&[request, response])?;
        self.notify.signal(REQUEST_QUEUE, self.request_notify)?;

        loop {
            self.poll_faults()?;
            if let Some(used) = self.request_queue.pop()? {
                if used.head() != head || used.written() < 4 {
                    return Err(VirtioError::Device);
                }
                let iommu_status = self.request.read_u8(status_at)?;
                return if iommu_status == STATUS_OK {
                    Ok(())
                } else {
                    Err(VirtioError::Iommu(iommu_status))
                };
            }
            // A zero answer is a polling fallback, not permission to reuse a
            // command buffer whose device ownership is still unresolved.
            let _ = self.arrivals.wait();
        }
    }
}

impl<A: Arrivals> Mapper for Iommu<'_, '_, A> {
    fn access_platform(&self) -> bool {
        true
    }

    fn map(
        &mut self,
        device: DeviceId,
        region: Region,
        perm: DmaPerm,
    ) -> Result<Mapping, MapError> {
        let len = region.len();
        let Some(domain) = self.domains.domain(device) else {
            return Err(MapError::new(DmaError::NotMapped, region));
        };
        if region.physical() % self.page != 0 || len == 0 || len % self.page != 0 {
            return Err(MapError::new(DmaError::Alignment, region));
        }
        let Some(slot) = self.active.iter().position(Option::is_none) else {
            return Err(MapError::new(DmaError::OutOfSpace, region));
        };
        let iova = match self.space.reserve(len, self.page) {
            Ok(iova) => iova,
            Err(error) => return Err(MapError::new(error, region)),
        };
        if encode_map(&self.request, domain, iova, region.physical(), len, perm)
            .and_then(|()| self.command(STATUS_AT_MAP))
            .is_err()
        {
            self.space.release(iova, len).ok();
            return Err(MapError::new(DmaError::Backend, region));
        }
        self.active[slot] = Some(Active { device, domain, iova, len, perm });
        // SAFETY: the completed MAP request installed this exact device,
        // address, length, physical backing, and permission tuple.
        Ok(unsafe { Mapping::from_backend(region, device, iova, perm) })
    }

    fn unmap(&mut self, mapping: Mapping) -> Result<Region, UnmapError> {
        let slot = self.active.iter().position(|entry| {
            entry.is_some_and(|entry| {
                entry.device == mapping.device()
                    && entry.iova == mapping.iova()
                    && entry.len == mapping.len()
                    && entry.perm == mapping.perm()
            })
        });
        let Some(slot) = slot else {
            return Err(UnmapError::new(DmaError::NotMapped, mapping));
        };
        let domain = self.active[slot].unwrap().domain;
        if encode_unmap(&self.request, domain, mapping.iova(), mapping.len())
            .and_then(|()| self.command(STATUS_AT_UNMAP))
            .is_err()
        {
            return Err(UnmapError::new(DmaError::Backend, mapping));
        }
        if let Err(error) = self.space.release(mapping.iova(), mapping.len()) {
            return Err(UnmapError::new(error, mapping));
        }
        self.active[slot] = None;
        // SAFETY: UNMAP completed and the IOVA reservation was removed.
        Ok(unsafe { mapping.into_region() })
    }
}

fn make_queue(
    common: &mut Common<'_>,
    index: u16,
    vector: u16,
    controller: DeviceId,
    arena: &mut Arena<'_>,
    minimum: u16,
) -> Result<(Queue, u16), VirtioError> {
    common.select_queue(index)?;
    let offered = common.queue_size()?;
    let size = queue_size(offered, minimum)?;
    let mut identity = Identity;
    let descriptors = identity_map(
        &mut identity,
        controller,
        arena.region(queue::descriptor_bytes(size))?,
        DmaPerm::READ,
    )?;
    let driver = identity_map(
        &mut identity,
        controller,
        arena.region(queue::driver_bytes(size))?,
        DmaPerm::READ,
    )?;
    let device = identity_map(
        &mut identity,
        controller,
        arena.region(queue::device_bytes(size))?,
        DmaPerm::READ_WRITE,
    )?;
    let queue = Queue::new(size, descriptors, driver, device)?;
    common.set_queue_size(size)?;
    common.set_queue_vector(vector)?;
    common.set_queue_rings(queue.descriptors_iova(), queue.driver_iova(), queue.device_iova())?;
    common.enable_queue()?;
    let notify = common.queue_notify_off()?;
    Ok((queue, notify))
}

fn identity_map(
    identity: &mut Identity,
    device: DeviceId,
    region: Region,
    perm: DmaPerm,
) -> Result<Mapping, VirtioError> {
    identity.map(device, region, perm).map_err(|error| VirtioError::Dma(error.error()))
}

fn validate_notify(notify: &Mmio<'_>, multiplier: u32, offset: u16) -> Result<(), VirtioError> {
    let at = offset as u64 * multiplier as u64;
    if at.checked_add(2).is_none_or(|end| end > notify.len()) {
        return Err(VirtioError::Device);
    }
    Ok(())
}

fn read_config(common: &Common<'_>, device: &Mmio<'_>) -> Result<Config, VirtioError> {
    for _ in 0..CONFIG_SPINS {
        let before = common.config_generation()?;
        let config = Config {
            page_size_mask: device.read_u64(0)?,
            input_start: device.read_u64(8)?,
            input_end: device.read_u64(16)?,
            domain_start: device.read_u32(24)?,
            domain_end: device.read_u32(28)?,
        };
        if common.config_generation()? == before {
            return Ok(config);
        }
    }
    Err(VirtioError::Device)
}

fn page_size(mask: u64) -> Result<u64, VirtioError> {
    if mask == 0 {
        return Err(VirtioError::Device);
    }
    Ok(1u64 << mask.trailing_zeros())
}

fn aperture(start: u64, inclusive_end: u64, page: u64) -> Result<(u64, u64), VirtioError> {
    let start = align_up(start.max(1), page).ok_or(VirtioError::Device)?;
    let preferred = align_up(PREFERRED_IOVA.max(start), page).ok_or(VirtioError::Device)?;
    let end = inclusive_end.checked_add(1).unwrap_or(inclusive_end & !(page - 1));
    let start = if preferred < end { preferred } else { start };
    if start >= end {
        return Err(VirtioError::Device);
    }
    Ok((start, end))
}

fn queue_size(offered: u16, minimum: u16) -> Result<u16, VirtioError> {
    let capped = offered.min(queue::MAX_SIZE);
    if capped < minimum {
        return Err(VirtioError::Device);
    }
    Ok(1 << (15 - capped.leading_zeros() as u16))
}

fn encode_attach(request: &Mapping, domain: u32, endpoint: DeviceId) -> Result<(), VirtioError> {
    request.zero();
    request.write_u8(0, ATTACH)?;
    request.write_u32(4, domain)?;
    request.write_u32(8, endpoint.get())?;
    Ok(())
}

fn encode_detach(request: &Mapping, domain: u32, endpoint: DeviceId) -> Result<(), VirtioError> {
    request.zero();
    request.write_u8(0, DETACH)?;
    request.write_u32(4, domain)?;
    request.write_u32(8, endpoint.get())?;
    Ok(())
}

fn encode_map(
    request: &Mapping,
    domain: u32,
    iova: Iova,
    physical: u64,
    len: u64,
    perm: DmaPerm,
) -> Result<(), VirtioError> {
    let end = iova.get().checked_add(len - 1).ok_or(VirtioError::Device)?;
    request.zero();
    request.write_u8(0, MAP)?;
    request.write_u32(4, domain)?;
    request.write_u64(8, iova.get())?;
    request.write_u64(16, end)?;
    request.write_u64(24, physical)?;
    let flags =
        if perm.can_read() { MAP_READ } else { 0 } | if perm.can_write() { MAP_WRITE } else { 0 };
    request.write_u32(32, flags)?;
    Ok(())
}

fn encode_unmap(request: &Mapping, domain: u32, iova: Iova, len: u64) -> Result<(), VirtioError> {
    let end = iova.get().checked_add(len - 1).ok_or(VirtioError::Device)?;
    request.zero();
    request.write_u8(0, UNMAP)?;
    request.write_u32(4, domain)?;
    request.write_u64(8, iova.get())?;
    request.write_u64(16, end)?;
    Ok(())
}

fn parse_fault(events: &Mapping, at: u64) -> Result<Fault, VirtioError> {
    Ok(Fault {
        reason: events.read_u8(at)?,
        flags: events.read_u32(at + 4)?,
        endpoint: DeviceId::new(events.read_u32(at + 8)?),
        address: events.read_u64(at + 16)?,
    })
}

#[cfg(test)]
mod tests {
    use molt_arch::dma::Region;
    use molt_arch::iommu::{DeviceId, DmaPerm, Identity, Iova, Mapper, Mapping};

    use super::{Domains, EVENT_BYTES, aperture, encode_map, parse_fault, queue_size};

    fn mapping(bytes: &mut [u8]) -> Mapping {
        // SAFETY: the array stays live and uniquely borrowed through the test.
        let region = unsafe { Region::new(bytes.as_mut_ptr(), 0x2000, bytes.len() as u64) };
        Identity.map(DeviceId::new(1), region, DmaPerm::READ_WRITE).ok().unwrap()
    }

    #[test]
    fn map_encoding() {
        let mut bytes = [0u8; 40];
        let request = mapping(&mut bytes);

        encode_map(&request, 7, Iova::new(0x1_0000_0000), 0x20_0000, 0x1000, DmaPerm::WRITE)
            .unwrap();

        assert_eq!(bytes[0], 3);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 7);
        assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), 0x1_0000_0fff);
        assert_eq!(u32::from_le_bytes(bytes[32..36].try_into().unwrap()), 2);
    }

    #[test]
    fn aperture_bounds() {
        assert_eq!(aperture(0, u64::MAX, 0x1000), Ok((0x1_0000_0000, !0xfff)));
        assert_eq!(aperture(0x2000, 0x9fff, 0x1000), Ok((0x2000, 0xa000)));
    }

    #[test]
    fn fault_parse() {
        let mut bytes = [0u8; EVENT_BYTES as usize];
        bytes[0] = 3;
        bytes[4..8].copy_from_slice(&5u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&0x18u32.to_le_bytes());
        bytes[16..24].copy_from_slice(&0xfeed_0000u64.to_le_bytes());
        let events = mapping(&mut bytes);

        let fault = parse_fault(&events, 0).unwrap();

        assert_eq!(fault.reason(), 3);
        assert_eq!(fault.flags(), 5);
        assert_eq!(fault.endpoint(), DeviceId::new(0x18));
        assert_eq!(fault.address(), 0xfeed_0000);
    }

    #[test]
    fn queue_bound() {
        assert_eq!(queue_size(256, 2), Ok(32));
        assert_eq!(queue_size(17, 2), Ok(16));
        assert!(queue_size(1, 2).is_err());
    }

    #[test]
    fn domains_isolate_endpoints() -> Result<(), crate::VirtioError> {
        let mut domains = Domains::<2>::new(7, 8);
        let first = DeviceId::new(0x18);
        let second = DeviceId::new(0x20);

        assert_eq!(domains.reserve(first)?, 7);
        assert_eq!(domains.reserve(second)?, 8);
        assert_eq!(domains.domain(first), Some(7));
        assert_eq!(domains.domain(second), Some(8));
        assert!(domains.reserve(DeviceId::new(0x28)).is_err());
        Ok(())
    }

    #[test]
    fn domain_reuses_release() -> Result<(), crate::VirtioError> {
        let mut domains = Domains::<2>::new(4, 5);
        let first = DeviceId::new(1);
        domains.reserve(first)?;
        domains.release(first)?;

        assert_eq!(domains.reserve(DeviceId::new(2))?, 4);
        Ok(())
    }
}
