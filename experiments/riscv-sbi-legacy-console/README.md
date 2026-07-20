# The legacy SBI console still boots the kernel

The debug console extension (DBCN) is only guaranteed by SBI v2.0, so the port
probes for it and keeps `console_putchar` as the fallback. QEMU's bundled
OpenSBI implements DBCN, which means the fallback would otherwise never run on
any CI host — an untested path that only firmware older than the developer's
would ever reach.

This experiment forces the probe to fail and checks the kernel boots anyway.

```
$ ./run.sh
injected: DBCN probe forced to fail
PASS: smoke exited 0 on the legacy console
MOLT_SBI_CONSOLE: Legacy
MOLT_BOOT_OK
```

`MOLT_SBI_CONSOLE:` is a required marker for RISC-V boot runs, but the smoke
runner deliberately does not require a particular backend: which one wins is a
property of the firmware, not of the kernel.

The script restores `sbi.rs` on exit, including on failure.
