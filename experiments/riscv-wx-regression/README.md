# The RISC-V image audit fails on a writable `.text`

`MOLT_WX_OK` is only worth printing if it can be absent. This experiment
reintroduces the defect the per-section mapping removed — `.text` mapped with
write rights, as the old RWX gigapage left it — and checks that the smoke run
reports it.

```
$ ./run.sh
injected: .text mapped writable
PASS: smoke exited 1
MOLT_MAPPING_OK
MOLT_PANIC: panicked at kernel/src/main.rs:58:49:
error: riscv64 boot QEMU exited without the MOLT_WX_OK serial marker
```

The audit walks the live Sv39 tables rather than remembering what was
requested, so it catches a mapping that was correct when it was made and
relaxed afterwards as well.

The script restores `paging.rs` on exit, including on failure.
