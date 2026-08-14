#![cfg_attr(target_os = "molt", no_std)]
#![cfg_attr(target_os = "molt", no_main)]

#[cfg(not(target_os = "molt"))]
fn main() {}

#[cfg(target_os = "molt")]
mod image {

    use core::cell::RefCell;

    use molt_core::buffer::BufferRegistry;
    use molt_core::capability::Capability;
    use molt_core::cell::{Cell, CellId};
    use molt_core::registry::Registry;
    use molt_core::ring::{Completion, IoDriver, IoRing};
    use molt_core::{CpuId, task};
    use molt_fs::{Dir, FsDone, FsError, FsOp, Handle as FsHandle, Mount, Storage};
    use molt_shell::{Console, Session, Shell};
    use molt_user::{Buffer, Client, Directory, File, Handle, Heap, Output, block_on, exit};

    const RING: usize = 4;
    const WINDOW: usize = 64;
    const FILE_RESULT: u64 = 1 << 62;
    const DIR_RESULT: u64 = 1 << 61;
    const SCRIPT: &[u8] = b"cat hello.txt";
    const SHELL: CellId = CellId::new(3);
    const FS: CellId = CellId::new(2);

    #[global_allocator]
    static HEAP: Heap = Heap::empty();

    #[unsafe(no_mangle)]
    #[unsafe(link_section = ".text._start")]
    extern "C" fn _start(
        channel: *const (),
        base: usize,
        output: u64,
        heap: usize,
        heap_len: usize,
        root: u64,
    ) -> ! {
        // SAFETY: bootstrap maps this writable range only into this domain.
        unsafe { HEAP.initialize(heap, heap_len) };
        // SAFETY: bootstrap initialized the shared channel before entering us.
        let client = RefCell::new(unsafe { Client::<RING>::from_raw(channel) });
        let output = Handle::<Output>::from_raw(output);

        let mut scratch_bytes = [0u8; WINDOW];
        let mut buffers = BufferRegistry::<1>::new();
        let scratch =
            buffers.register_read_write(SHELL, &mut scratch_bytes).unwrap_or_else(|_| exit(2));
        let buffers = RefCell::new(buffers);

        let mut names = Registry::<Storage, 1>::new();
        // SAFETY: bootstrap supplied a filesystem root issued by the kernel table;
        // the kernel validates it again when the operation crosses the ABI.
        let root = unsafe { Capability::<Dir>::from_raw(root) };
        names.publish(FS, CpuId::BOOT, Mount::new(root, 0)).unwrap_or_else(|_| exit(3));
        let names = RefCell::new(names);

        let mut ring = IoRing::<FsOp, Result<FsDone, FsError>, RING>::new();
        let (fs_client, mut fs_driver) = ring.split();
        let session =
            Session::new(fs_client, &buffers, &names, scratch, WINDOW).unwrap_or_else(|_| exit(4));
        let mut shell = Shell::spawn(session).unwrap_or_else(|_| exit(5));
        let mut console = DomainConsole { client: &client, base, output };

        let result = task::drive(shell.script(SCRIPT, &mut console), || {
            serve(&client, base, &mut fs_driver, &buffers);
        });
        if result.is_err() {
            exit(6);
        }
        exit(0)
    }

    struct DomainConsole<'a> {
        client: &'a RefCell<Client<'a, RING>>,
        base: usize,
        output: Handle<Output>,
    }

    impl Console for DomainConsole<'_> {
        fn write(&mut self, bytes: &[u8]) {
            let Some(buffer) = Buffer::from_slice(self.base, bytes) else {
                exit(7);
            };
            let mut client = self.client.borrow_mut();
            if block_on(client.write(self.output, buffer)).is_err() {
                exit(8);
            }
        }
    }

    fn serve(
        client: &RefCell<Client<'_, RING>>,
        base: usize,
        driver: &mut IoDriver<'_, FsOp, Result<FsDone, FsError>, RING>,
        buffers: &RefCell<BufferRegistry<'_, 1>>,
    ) {
        let Some(submission) = driver.try_next() else {
            return;
        };
        let id = submission.id();
        let result = match submission.into_operation() {
            FsOp::Open { dir, name } => {
                let Some(name) = Buffer::from_slice(base, name.as_bytes()) else {
                    return complete(
                        driver,
                        id,
                        Err(FsError::Buffer(molt_core::buffer::BufferError::OutOfBounds)),
                    );
                };
                let mut client = client.borrow_mut();
                remote(block_on(client.open(Handle::<Directory>::from_raw(dir.raw()), name)))
                    .and_then(|raw| {
                        let raw = raw as u64;
                        if raw & FILE_RESULT != 0 {
                            // SAFETY: this typed value came from the kernel's open completion.
                            Ok(FsDone::Opened(FsHandle::File(unsafe {
                                Capability::from_raw(raw & !FILE_RESULT)
                            })))
                        } else if raw & DIR_RESULT != 0 {
                            // SAFETY: this typed value came from the kernel's open completion.
                            Ok(FsDone::Opened(FsHandle::Dir(unsafe {
                                Capability::from_raw(raw & !DIR_RESULT)
                            })))
                        } else {
                            Err(FsError::Corrupt)
                        }
                    })
            }
            FsOp::Read { file, buffer, offset } => {
                let mut buffers = buffers.borrow_mut();
                match buffers.resolve_write(buffer) {
                    Ok(bytes) => {
                        let Some(buffer) = Buffer::from_mut_slice(base, bytes) else {
                            return complete(driver, id, Err(FsError::Range));
                        };
                        let mut client = client.borrow_mut();
                        remote(block_on(client.read(
                            Handle::<File>::from_raw(file.raw()),
                            offset,
                            buffer,
                        )))
                        .map(|read| FsDone::Read(read as usize))
                    }
                    Err(error) => Err(FsError::Buffer(error)),
                }
            }
            FsOp::Close(handle) => {
                let raw = match handle {
                    FsHandle::Dir(handle) => handle.raw(),
                    FsHandle::File(handle) => handle.raw(),
                };
                let mut client = client.borrow_mut();
                remote(block_on(client.close(Handle::<File>::from_raw(raw))))
                    .map(|_| FsDone::Closed)
            }
            _ => Err(FsError::Corrupt),
        };
        complete(driver, id, result);
    }

    fn remote(result: Result<i64, molt_user::Error>) -> Result<i64, FsError> {
        result
            .map_err(|_| FsError::Corrupt)
            .and_then(|value| if value < 0 { Err(FsError::Corrupt) } else { Ok(value) })
    }

    fn complete(
        driver: &mut IoDriver<'_, FsOp, Result<FsDone, FsError>, RING>,
        id: molt_core::ring::RequestId,
        result: Result<FsDone, FsError>,
    ) {
        if driver.try_complete(Completion::new(id, result)).is_err() {
            exit(9);
        }
    }

    #[panic_handler]
    fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
        exit(101)
    }
}
