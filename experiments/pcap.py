#!/usr/bin/env python3
"""Prints the Ethernet/IP/TCP summary of a QEMU filter-dump capture.

Used to see whether a frame the guest believed it sent ever reached the wire
during the x86_64 TCP smoke.
"""

import struct
import sys


def frames(path):
    with open(path, "rb") as handle:
        blob = handle.read()
    magic, = struct.unpack_from("<I", blob, 0)
    endian = "<" if magic in (0xA1B2C3D4, 0xA1B23C4D) else ">"
    offset = 24
    while offset + 16 <= len(blob):
        sec, usec, caplen, _ = struct.unpack_from(endian + "IIII", blob, offset)
        offset += 16
        yield sec + usec / 1e6, blob[offset:offset + caplen]
        offset += caplen


def flags(bits):
    names = [(0x02, "SYN"), (0x10, "ACK"), (0x01, "FIN"), (0x04, "RST"), (0x08, "PSH")]
    return "|".join(name for bit, name in names if bits & bit) or "-"


def summary(frame):
    kind, = struct.unpack_from("!H", frame, 12)
    if kind == 0x0806:
        op, = struct.unpack_from("!H", frame, 20)
        src = ".".join(str(byte) for byte in frame[28:32])
        dst = ".".join(str(byte) for byte in frame[38:42])
        return f"ARP {'request' if op == 1 else 'reply'} {src} -> {dst}"
    if kind == 0x86DD:
        return f"IPv6 next {frame[20]}"
    if kind != 0x0800:
        return f"ethertype {kind:#06x}"
    ihl = (frame[14] & 0xF) * 4
    protocol = frame[23]
    src = ".".join(str(byte) for byte in frame[26:30])
    dst = ".".join(str(byte) for byte in frame[30:34])
    body = 14 + ihl
    if protocol == 6:
        sport, dport, seq, ack = struct.unpack_from("!HHII", frame, body)
        data = (frame[body + 12] >> 4) * 4
        total, = struct.unpack_from("!H", frame, 16)
        payload = total - ihl - data
        bits = flags(frame[body + 13])
        return f"TCP {src}:{sport} -> {dst}:{dport} {bits} seq {seq} ack {ack} len {payload}"
    if protocol == 17:
        sport, dport = struct.unpack_from("!HH", frame, body)
        return f"UDP {src}:{sport} -> {dst}:{dport}"
    return f"IPv4 protocol {protocol} {src} -> {dst}"


def main():
    for path in sys.argv[1:]:
        print(path)
        first = None
        for stamp, frame in frames(path):
            first = first if first is not None else stamp
            print(f"  {stamp - first:8.3f} {summary(frame)}")


if __name__ == "__main__":
    main()
