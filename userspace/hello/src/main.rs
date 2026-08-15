#![cfg_attr(target_os = "molt", no_std)]
#![cfg_attr(target_os = "molt", no_main)]

#[cfg(not(target_os = "molt"))]
fn main() {}

#[cfg(target_os = "molt")]
mod image {

    use molt_program::{Buffer, Client, Handle, Heap, Output, block_on, exit};

    const RING: usize = 4;
    const MESSAGE: &[u8] = b"hello from a Molt domain\n";

    /// Asks for the spin probe rather than the ring, in the argument the fault
    /// probe already overloads.
    const SPIN: u64 = u64::MAX - 1;

    /// Chosen to outrun the 10 ms tick under emulation, where the smoke runs.
    const SPIN_ITERATIONS: u64 = 8_000_000;

    #[global_allocator]
    static HEAP: Heap = Heap::empty();

    /// Starts with only the shared ring, the domain base, and delegated output.
    #[unsafe(no_mangle)]
    #[unsafe(link_section = ".text._start")]
    extern "C" fn _start(
        channel: *const (),
        base: usize,
        output: u64,
        heap: usize,
        heap_len: usize,
    ) -> ! {
        // SAFETY: the kernel maps and zeroes this heap exclusively for the domain
        // before entering it, and never re-enters after `exit`.
        unsafe { HEAP.initialize(heap, heap_len) };
        if output == u64::MAX {
            // SAFETY: this deliberately touches an unmapped address so the boot
            // smoke can prove the processor returns the fault to the kernel
            // without taking the kernel or another domain down with it.
            unsafe { core::ptr::read_volatile(0x1000 as *const u8) };
        }
        if output == SPIN {
            // Long enough to outlast a scheduling tick on either port. A domain
            // is not preemptible, so the only correct outcome is this exit: a
            // tick delivered here would come back as a fault instead.
            for _ in 0..SPIN_ITERATIONS {
                core::hint::spin_loop();
            }
            exit(0)
        }
        // SAFETY: bootstrap passes a page-aligned, initialized Channel<RING> which
        // stays mapped for this image's lifetime.
        let mut client = unsafe { Client::<RING>::from_raw(channel) };
        let output = Handle::<Output>::from_raw(output);
        let message = Buffer::from_slice(base, MESSAGE).unwrap_or_else(|| exit(2));

        if block_on(client.write(output, message)).is_err() || block_on(client.timer(1)).is_err() {
            exit(3);
        }
        exit(0)
    }

    #[panic_handler]
    fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
        exit(101)
    }
}
