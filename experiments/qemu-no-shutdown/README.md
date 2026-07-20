# `-no-shutdown` turns a guest reset into a hang

Reproduction for the `just smoke-x86_64` failure reported in issue #14:

```
== smoke: x86_64 boot ==
error: QEMU did not exit within 20s
```

The report has no serial output at all, which is the first clue: the old smoke
runner collected QEMU's output only *after* a successful wait, so a timeout
threw the guest's log away.

The second clue is the flag set. The runner passed both `-no-reboot` and
`-no-shutdown`. QEMU documents `-no-shutdown` as "do not exit QEMU on guest
shutdown, but instead only stop the emulation", so once the guest asks for a
reset — which is what a triple fault during early boot becomes — QEMU parks
with `-display none` and no monitor attached, and nothing ever exits it.

`run.sh` boots a 512-byte guest that loads a null IDT and executes `int3`, i.e.
triple-faults immediately, and runs it under the two flag sets:

```console
$ ./run.sh
with    -no-shutdown: timed out after 20s (exit 124)
without -no-shutdown: exited with 0
```

So the flag, not the guest, decides between "reported failure" and "hang". The
fix in `xtask` is to drop `-no-shutdown` from the smoke path (the interactive
`cargo boot` path keeps it, where a monitor can use it), drain the serial pipe
on its own thread, and print whatever the guest said even when the run times
out.

This does not by itself explain *why* the kernel resets on some hosts and boots
on others — an Ubuntu 24.04 host with QEMU 8.2.2 passes the smoke test as-is.
It does mean that the same failure now prints the boot log and a real exit
status instead of a bare timeout.
