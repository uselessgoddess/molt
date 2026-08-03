//! One bounded NVMe namespace behind a translated DMA address space.
//!
//! [`Prepared::prepare`] disables the controller, allocates every queue and
//! payload page, and maps them for one PCI requester. The caller may only then
//! grant bus-master authority and call [`Prepared::enable`]. This two-phase
//! boundary makes the unsafe ordering visible in the API instead of relying on
//! a comment beside a PCI command-register write.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod queue;

use core::sync::atomic::{Ordering, fence};

use molt_arch::dma::{Arena, DmaError, Region};
use molt_arch::iommu::{DeviceId, DmaPerm, Mapper, Mapping};
use molt_arch::{Mmio, MmioError};

use crate::queue::Parts;
pub use crate::queue::{Nvme, QUEUE_DEPTH};

const CAP: u64 = 0x00;
const VERSION: u64 = 0x08;
const CC: u64 = 0x14;
const CSTS: u64 = 0x1c;
const AQA: u64 = 0x24;
const ASQ: u64 = 0x28;
const ACQ: u64 = 0x30;
pub(crate) const DOORBELL: u64 = 0x1000;

const CC_ENABLE: u32 = 1;
const CC_LAYOUT: u32 = 6 << 16 | 4 << 20;
const CSTS_READY: u32 = 1;
const CSTS_FATAL: u32 = 1 << 1;
const CAP_NVM: u64 = 1 << 37;
const CAP_TIMEOUT_SHIFT: u32 = 24;
const CAP_STRIDE_SHIFT: u32 = 32;
const CAP_MPS_MIN_SHIFT: u32 = 48;
const CAP_MPS_MAX_SHIFT: u32 = 52;
const PAGE: u64 = 4096;
const ADMIN_ENTRIES: u16 = 16;
pub(crate) const IO_ENTRIES: u16 = QUEUE_DEPTH as u16 + 1;
const READY_SPINS: u64 = 2_000_000;

const IDENTIFY: u8 = 0x06;
const CREATE_IO_SQ: u8 = 0x01;
const CREATE_IO_CQ: u8 = 0x05;
const SET_FEATURES: u8 = 0x09;
const FEATURE_QUEUES: u32 = 0x07;

/// Why the controller could not complete an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NvmeError {
    /// A register access left its BAR window.
    Mmio(MmioError),
    /// A DMA region or translation was invalid.
    Dma(DmaError),
    /// The controller or namespace uses a layout this bounded driver lacks.
    Unsupported,
    /// The controller returned a failed or malformed completion.
    Device,
    /// The controller did not change state within its advertised budget.
    Timeout,
}

impl From<MmioError> for NvmeError {
    fn from(error: MmioError) -> Self {
        Self::Mmio(error)
    }
}

impl From<DmaError> for NvmeError {
    fn from(error: DmaError) -> Self {
        Self::Dma(error)
    }
}

/// What an NVMe queue waits on after a completion poll misses.
pub trait Arrivals {
    /// Returns arrivals since the previous wait, or zero on expiry.
    fn wait(&mut self) -> u64;
}

/// Everything needed to prepare one controller while it cannot initiate DMA.
pub struct Config<'window, A> {
    registers: Mmio<'window>,
    endpoint: DeviceId,
    vector: u16,
    arrivals: A,
}

impl<'window, A> Config<'window, A> {
    pub const fn new(
        registers: Mmio<'window>,
        endpoint: DeviceId,
        vector: u16,
        arrivals: A,
    ) -> Self {
        Self { registers, endpoint, vector, arrivals }
    }
}

struct Resources {
    admin_sq: Mapping,
    admin_cq: Mapping,
    identify: Mapping,
    io_sq: Mapping,
    io_cq: Mapping,
    data: Mapping,
}

impl Resources {
    fn into_mappings(self) -> [Mapping; 6] {
        [self.admin_sq, self.admin_cq, self.identify, self.io_sq, self.io_cq, self.data]
    }
}

/// A disabled controller whose complete DMA footprint is already mapped.
pub struct Prepared<'slots, 'window, A, M> {
    registers: Mmio<'window>,
    endpoint: DeviceId,
    vector: u16,
    arrivals: A,
    mapper: M,
    arena: Arena<'slots>,
    resources: Resources,
    stride: u64,
}

impl<'slots, 'window, A: Arrivals, M: Mapper> Prepared<'slots, 'window, A, M> {
    /// Maps all controller memory while the PCI function is still quiesced.
    pub fn prepare(
        config: Config<'window, A>,
        mut mapper: M,
        mut arena: Arena<'slots>,
    ) -> Result<Self, NvmeError> {
        let Config { registers, endpoint, vector, arrivals } = config;
        let cap = registers.read_u64(CAP)?;
        validate_capabilities(cap, registers.read_u32(VERSION)?)?;

        registers.write_u32(CC, 0)?;
        wait_ready(&registers, cap, false)?;

        let admin_sq = mapped(&mut mapper, endpoint, arena.region(PAGE)?, DmaPerm::READ)?;
        let admin_cq = mapped(&mut mapper, endpoint, arena.region(PAGE)?, DmaPerm::WRITE)?;
        let identify = mapped(&mut mapper, endpoint, arena.region(PAGE)?, DmaPerm::WRITE)?;
        let io_sq = mapped(&mut mapper, endpoint, arena.region(PAGE)?, DmaPerm::READ)?;
        let io_cq = mapped(&mut mapper, endpoint, arena.region(PAGE)?, DmaPerm::WRITE)?;
        let data = mapped(
            &mut mapper,
            endpoint,
            arena.region(PAGE * QUEUE_DEPTH as u64)?,
            DmaPerm::READ_WRITE,
        )?;

        admin_sq.zero();
        admin_cq.zero();
        identify.zero();
        io_sq.zero();
        io_cq.zero();
        data.zero();
        registers
            .write_u32(AQA, (ADMIN_ENTRIES as u32 - 1) | ((ADMIN_ENTRIES as u32 - 1) << 16))?;
        registers.write_u64(ASQ, admin_sq.iova().get())?;
        registers.write_u64(ACQ, admin_cq.iova().get())?;

        let resources = Resources { admin_sq, admin_cq, identify, io_sq, io_cq, data };
        let stride = 4u64 << ((cap >> CAP_STRIDE_SHIFT) & 0xf);
        Ok(Self { registers, endpoint, vector, arrivals, mapper, arena, resources, stride })
    }

    /// Enables the controller after the caller has granted bus mastering.
    pub fn enable(mut self) -> Result<Nvme<'slots, 'window, A, M>, NvmeError> {
        let cap = self.registers.read_u64(CAP)?;
        self.registers.write_u32(CC, CC_LAYOUT | CC_ENABLE)?;
        wait_ready(&self.registers, cap, true)?;

        let namespace = {
            let mut admin = Admin::new(
                &self.registers,
                &self.resources.admin_sq,
                &self.resources.admin_cq,
                &self.resources.identify,
                self.stride,
                &mut self.arrivals,
            );
            admin.identify_controller()?;
            let namespace = admin.identify_namespace()?;
            admin.set_one_queue_pair()?;
            admin.create_io_cq(self.resources.io_cq.iova().get(), self.vector)?;
            admin.create_io_sq(self.resources.io_sq.iova().get())?;
            namespace
        };

        let Self { registers, endpoint, arrivals, mapper, arena, resources, stride, .. } = self;
        Ok(Nvme::from_parts(Parts {
            registers,
            endpoint,
            arrivals,
            mapper,
            arena,
            resources,
            namespace,
            stride,
        }))
    }
}

fn mapped(
    mapper: &mut impl Mapper,
    endpoint: DeviceId,
    region: Region,
    perm: DmaPerm,
) -> Result<Mapping, NvmeError> {
    mapper.map(endpoint, region, perm).map_err(|error| NvmeError::Dma(error.error()))
}

fn validate_capabilities(cap: u64, version: u32) -> Result<(), NvmeError> {
    let queue_entries = (cap & 0xffff) + 1;
    let min_page = (cap >> CAP_MPS_MIN_SHIFT) & 0xf;
    let max_page = (cap >> CAP_MPS_MAX_SHIFT) & 0xf;
    if version < 0x0001_0000
        || cap & CAP_NVM == 0
        || queue_entries < ADMIN_ENTRIES as u64
        || min_page > 0
        || max_page < min_page
    {
        return Err(NvmeError::Unsupported);
    }
    Ok(())
}

fn wait_ready(registers: &Mmio<'_>, cap: u64, ready: bool) -> Result<(), NvmeError> {
    let advertised = (cap >> CAP_TIMEOUT_SHIFT) & 0xff;
    let spins = READY_SPINS.saturating_mul(advertised.max(1));
    for _ in 0..spins {
        let status = registers.read_u32(CSTS)?;
        if status & CSTS_FATAL != 0 {
            return Err(NvmeError::Device);
        }
        if (status & CSTS_READY != 0) == ready {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(NvmeError::Timeout)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Namespace {
    blocks: u64,
    shift: u8,
}

impl Namespace {
    pub(crate) fn sectors(self) -> u64 {
        self.blocks.checked_shl((self.shift - 9) as u32).unwrap_or(0)
    }

    pub(crate) const fn bytes(self) -> usize {
        1usize << self.shift
    }

    pub(crate) const fn sectors_per_block(self) -> u64 {
        1u64 << (self.shift - 9)
    }
}

fn namespace(identify: &Mapping) -> Result<Namespace, NvmeError> {
    let blocks = identify.read_u64(0)?;
    let flbas = identify.read_u8(26)?;
    let format = flbas & 0x0f;
    if blocks == 0 || flbas & 0x60 != 0 || identify.read_u8(29)? != 0 {
        return Err(NvmeError::Unsupported);
    }
    let at = 128 + format as u64 * 4;
    let metadata = identify.read_u16(at)?;
    let shift = identify.read_u8(at + 2)?;
    if metadata != 0 || !(9..=12).contains(&shift) {
        return Err(NvmeError::Unsupported);
    }
    let namespace = Namespace { blocks, shift };
    if namespace.sectors() == 0 {
        return Err(NvmeError::Unsupported);
    }
    Ok(namespace)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Command {
    opcode: u8,
    cid: u16,
    namespace: u32,
    prp1: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
}

impl Command {
    pub(crate) const fn io(opcode: u8, cid: u16, lba: u64, blocks: u16, prp1: u64) -> Self {
        Self {
            opcode,
            cid,
            namespace: 1,
            prp1,
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: blocks as u32 - 1,
        }
    }
}

pub(crate) fn write_command(queue: &Mapping, slot: u16, command: Command) -> Result<(), NvmeError> {
    let at = slot as u64 * 64;
    for offset in (0..64).step_by(8) {
        queue.write_u64(at + offset, 0)?;
    }
    queue.write_u8(at, command.opcode)?;
    queue.write_u16(at + 2, command.cid)?;
    queue.write_u32(at + 4, command.namespace)?;
    queue.write_u64(at + 24, command.prp1)?;
    queue.write_u32(at + 40, command.cdw10)?;
    queue.write_u32(at + 44, command.cdw11)?;
    queue.write_u32(at + 48, command.cdw12)?;
    Ok(())
}

struct Admin<'a, 'window, A> {
    registers: &'a Mmio<'window>,
    sq: &'a Mapping,
    cq: &'a Mapping,
    identify: &'a Mapping,
    stride: u64,
    arrivals: &'a mut A,
    tail: u16,
    head: u16,
    phase: bool,
    cid: u16,
}

impl<'a, 'window, A: Arrivals> Admin<'a, 'window, A> {
    fn new(
        registers: &'a Mmio<'window>,
        sq: &'a Mapping,
        cq: &'a Mapping,
        identify: &'a Mapping,
        stride: u64,
        arrivals: &'a mut A,
    ) -> Self {
        Self {
            registers,
            sq,
            cq,
            identify,
            stride,
            arrivals,
            tail: 0,
            head: 0,
            phase: true,
            cid: 0,
        }
    }

    fn identify_controller(&mut self) -> Result<(), NvmeError> {
        self.identify.zero();
        let command = Command {
            opcode: IDENTIFY,
            namespace: 0,
            prp1: self.identify.iova().get(),
            cdw10: 1,
            ..Command::default()
        };
        self.submit(command)?;
        if self.identify.read_u32(516)? == 0 {
            return Err(NvmeError::Unsupported);
        }
        Ok(())
    }

    fn identify_namespace(&mut self) -> Result<Namespace, NvmeError> {
        self.identify.zero();
        let command = Command {
            opcode: IDENTIFY,
            namespace: 1,
            prp1: self.identify.iova().get(),
            cdw10: 0,
            ..Command::default()
        };
        self.submit(command)?;
        namespace(self.identify)
    }

    fn set_one_queue_pair(&mut self) -> Result<(u32, u32), NvmeError> {
        let command =
            Command { opcode: SET_FEATURES, cdw10: FEATURE_QUEUES, cdw11: 0, ..Command::default() };
        Ok(queue_pair_result(self.submit(command)?))
    }

    fn create_io_cq(&mut self, address: u64, vector: u16) -> Result<(), NvmeError> {
        let command = Command {
            opcode: CREATE_IO_CQ,
            prp1: address,
            cdw10: 1 | ((IO_ENTRIES as u32 - 1) << 16),
            cdw11: 0b11 | (u32::from(vector) << 16),
            ..Command::default()
        };
        self.submit(command).map(|_| ())
    }

    fn create_io_sq(&mut self, address: u64) -> Result<(), NvmeError> {
        let command = Command {
            opcode: CREATE_IO_SQ,
            prp1: address,
            cdw10: 1 | ((IO_ENTRIES as u32 - 1) << 16),
            cdw11: 1 | (1 << 16),
            ..Command::default()
        };
        self.submit(command).map(|_| ())
    }

    fn submit(&mut self, mut command: Command) -> Result<u32, NvmeError> {
        command.cid = self.cid;
        let expected = self.cid;
        self.cid = self.cid.wrapping_add(1);
        write_command(self.sq, self.tail, command)?;
        fence(Ordering::Release);
        self.tail = (self.tail + 1) % ADMIN_ENTRIES;
        self.registers.write_u32(DOORBELL, u32::from(self.tail))?;

        let mut expired = false;
        loop {
            let status = self.cq.read_u16(self.head as u64 * 16 + 14)?;
            if (status & 1 != 0) == self.phase {
                fence(Ordering::Acquire);
                let at = self.head as u64 * 16;
                let cid = self.cq.read_u16(at + 12)?;
                let result = self.cq.read_u32(at)?;
                self.advance()?;
                if cid != expected || status >> 1 != 0 {
                    return Err(NvmeError::Device);
                }
                return Ok(result);
            }
            if expired {
                return Err(NvmeError::Timeout);
            }
            expired = self.arrivals.wait() == 0;
        }
    }

    fn advance(&mut self) -> Result<(), NvmeError> {
        self.head += 1;
        if self.head == ADMIN_ENTRIES {
            self.head = 0;
            self.phase = !self.phase;
        }
        self.registers.write_u32(DOORBELL + self.stride, u32::from(self.head)).map_err(Into::into)
    }
}

fn queue_pair_result(allocated: u32) -> (u32, u32) {
    // Both fields are zero-based counts, so every successful completion
    // grants at least one queue even when the controller reports its maximum.
    ((allocated & 0xffff) + 1, (allocated >> 16) + 1)
}

#[cfg(test)]
mod tests {
    use molt_arch::dma::Region;
    use molt_arch::iommu::{DeviceId, DmaPerm, Identity, Mapper, Mapping};

    use super::{Command, Namespace, namespace, queue_pair_result, write_command};

    #[repr(align(4096))]
    struct Page([u8; 4096]);

    fn mapping(bytes: &mut [u8], physical: u64, perm: DmaPerm) -> Mapping {
        // SAFETY: the array stays live and uniquely models a DMA region.
        let region = unsafe { Region::new(bytes.as_mut_ptr(), physical, bytes.len() as u64) };
        Identity.map(DeviceId::new(1), region, perm).ok().unwrap()
    }

    #[test]
    fn command_encode() {
        let mut page = Page([0; 4096]);
        let bytes = &mut page.0;
        let queue = mapping(bytes, 0x1000, DmaPerm::READ);

        write_command(&queue, 0, Command::io(0x02, 7, 0x1122_3344_5566_7788, 8, 0x9000)).unwrap();

        assert_eq!(bytes[0], 0x02);
        assert_eq!(u16::from_le_bytes(bytes[2..4].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(bytes[24..32].try_into().unwrap()), 0x9000);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 0x5566_7788);
        assert_eq!(u32::from_le_bytes(bytes[44..48].try_into().unwrap()), 0x1122_3344);
        assert_eq!(u32::from_le_bytes(bytes[48..52].try_into().unwrap()), 7);
    }

    #[test]
    fn namespace_4k() {
        let mut page = Page([0; 4096]);
        let bytes = &mut page.0;
        bytes[..8].copy_from_slice(&32u64.to_le_bytes());
        bytes[26] = 1;
        bytes[132..134].copy_from_slice(&0u16.to_le_bytes());
        bytes[134] = 12;
        let identify = mapping(bytes, 0x1000, DmaPerm::WRITE);

        assert_eq!(namespace(&identify), Ok(Namespace { blocks: 32, shift: 12 }));
        assert_eq!(namespace(&identify).unwrap().sectors(), 256);
    }

    #[test]
    fn metadata_rejected() {
        let mut page = Page([0; 4096]);
        let bytes = &mut page.0;
        bytes[..8].copy_from_slice(&32u64.to_le_bytes());
        bytes[128..130].copy_from_slice(&8u16.to_le_bytes());
        bytes[130] = 9;
        let identify = mapping(bytes, 0x1000, DmaPerm::WRITE);

        assert!(namespace(&identify).is_err());
    }

    #[test]
    fn extra_queues_ok() {
        assert_eq!(queue_pair_result(0x003f_003f), (64, 64));
    }
}
