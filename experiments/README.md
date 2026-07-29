# experiments

Throwaway host-side tooling, kept because it is how a bug was found.

- `tcp-echo-loop.sh` — boots the x86_64 image N times and reports how long the
  TCP echo took, since a flake shows up as a distribution rather than a run.
- `pcap.py` — summarizes a QEMU `filter-dump` capture, to tell a frame the guest
  believed it sent from one that reached the wire.

`tcp-echo-loop.sh 20 /tmp/molt` caught the single-slot TX staging in
`molt_virtio::Net`: a capture of a failing run ended at the handshake's ACK,
with the data segment the guest had already reported as sent nowhere on it.
