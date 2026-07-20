#!/usr/bin/env bash
# Boots a guest that triple-faults immediately, with and without -no-shutdown.
set -euo pipefail

qemu=${MOLT_QEMU:-qemu-system-x86_64}
image=$(mktemp -t molt-triple-fault-XXXXXX.img)
trap 'rm -f "$image"' EXIT

python3 - "$image" <<'PY'
import sys

# 16-bit boot sector at 0x7c00: mask interrupts, load a null IDT, then int3.
# With no IDT entry and no way to report the fault, the CPU triple-faults and
# the machine resets.
sector = bytearray(512)
sector[0:7] = bytes([0xfa, 0x0f, 0x01, 0x1e, 0x10, 0x7c, 0xcc])
sector[0x10:0x16] = b"\x00" * 6  # the null IDT descriptor loaded above
sector[510:512] = b"\x55\xaa"
with open(sys.argv[1], "wb") as image:
    image.write(bytes(sector) + b"\x00" * (1024 * 1024 - 512))
PY

run() {
    local label=$1
    shift
    set +e
    timeout 20 "$qemu" -display none -no-reboot "$@" \
        -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
        -drive "format=raw,file=$image" -serial none >/dev/null 2>&1
    local status=$?
    set -e
    if [ "$status" -eq 124 ]; then
        echo "$label: timed out after 20s (exit 124)"
    else
        echo "$label: exited with $status"
    fi
}

run "with    -no-shutdown" -no-shutdown
run "without -no-shutdown"
