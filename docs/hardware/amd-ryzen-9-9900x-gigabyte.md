# Stage 1 hardware validation record

Target: AMD Ryzen 9 9900X on the issue owner's Gigabyte AM5 motherboard.

Status: **pending owner validation**. The motherboard model, firmware version,
and successful serial transcript have not yet been supplied, so this document
does not claim a physical boot.

## Machine record

| Field | Value |
| --- | --- |
| CPU | AMD Ryzen 9 9900X |
| Motherboard | Gigabyte AM5; exact model pending |
| Firmware version | pending |
| Firmware mode | UEFI, CSM disabled |
| Validation date | pending |
| Commit | pending |
| Result | pending |

## Reproduction procedure

1. On the development machine, check out the recorded commit and run `just
   check`, `just image`, and `just smoke`.
2. Connect the target's COM1-compatible serial output at 115200 baud, 8 data
   bits, no parity, and 1 stop bit. If the motherboard has no physical COM1
   header, record the tested PCIe or USB serial solution and its firmware
   behavior in the machine table.
3. Write `target/molt/molt-uefi.img` to a disposable USB drive. Verify the device
   path first: this operation destroys the selected drive's partition table.
4. Disable Secure Boot, select the USB drive's UEFI entry, and capture the full
   serial transcript from power-on through termination.
5. A passing Stage 1 run contains these lines in order:

   ```text
   MOLT_EXCEPTION_OK
   MOLT_MAPPING_OK
   MOLT_TIMER_OK
   MOLT_CANCELLATION_OK
   MOLT_STALE_COMPLETION_OK
   MOLT_RESTART_OK
   MOLT_BOOT_OK
   ```

6. Fill every pending field above and attach the serial transcript to the pull
   request. Record any firmware setting needed beyond the ones listed here.

QEMU is the repeatable automated acceptance environment. This physical-machine
record closes the separate hardware-compatibility item only after a real run.
