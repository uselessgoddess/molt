#!/usr/bin/env bash
# Boots the x86_64 image `runs` times and times each TCP echo.
#
#   experiments/tcp-echo-loop.sh [runs] [capture-prefix]
#
# With a prefix, every run is captured to `<prefix>-N.pcap` and the captures of
# runs that passed are deleted, leaving only the ones worth reading.
set -u

runs=${1:-20}
prefix=${2:-}
cargo xtask image >/dev/null || exit 1

for run in $(seq 1 "$runs"); do
    dump=()
    if [ -n "$prefix" ]; then
        dump=(-object "filter-dump,id=dump0,netdev=molt-net,file=$prefix-$run.pcap")
    fi
    out=$(timeout 60 qemu-system-x86_64 -machine q35 -display none -no-reboot \
        -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
        -drive format=raw,file=target/molt/molt-bios.img \
        -drive if=none,id=molt-disk,format=raw,file=target/molt/molt-disk.img \
        -device virtio-blk-pci,drive=molt-disk,disable-legacy=on \
        -netdev user,id=molt-net,guestfwd=tcp:10.0.2.100:80-cmd:cat \
        -device virtio-net-pci,netdev=molt-net,disable-legacy=on,mac=52:54:00:12:34:56 \
        "${dump[@]}" -serial stdio 2>&1 |
        while IFS= read -r line; do printf '%s %s\n' "$(date +%s.%N)" "$line"; done)

    if echo "$out" | grep -aq MOLT_BOOT_OK; then
        echo "$out" | awk '/MOLT_NDP_OK/{a=$1} /MOLT_TCP_OK/{b=$1}
                           END{printf "ok '"$run"' echo %.3fs\n", b - a}'
        if [ -n "$prefix" ]; then rm -f "$prefix-$run.pcap"; fi
    else
        echo "fail $run"
        echo "$out" | tail -2
    fi
done
