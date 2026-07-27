//! Internet one's-complement checksums.

/// Computes an Internet checksum over one byte slice.
pub fn compute(bytes: &[u8]) -> u16 {
    compute_parts(&[bytes])
}

/// Computes an Internet checksum without joining adjacent protocol headers.
pub fn compute_parts(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    let mut high = None;

    for part in parts {
        for &byte in *part {
            match high.take() {
                Some(first) => sum += u16::from_be_bytes([first, byte]) as u32,
                None => high = Some(byte),
            }
        }
    }
    if let Some(byte) = high {
        sum += (byte as u32) << 8;
    }
    while sum > u16::MAX as u32 {
        sum = (sum & u16::MAX as u32) + (sum >> 16);
    }
    !(sum as u16)
}

/// Writes a checksum into the two-byte field at `offset`.
pub fn set(bytes: &mut [u8], offset: usize) {
    let Some(end) = offset.checked_add(2) else {
        return;
    };
    if end > bytes.len() {
        return;
    }
    bytes[offset..end].fill(0);
    let value = compute(bytes);
    bytes[offset..end].copy_from_slice(&value.to_be_bytes());
}

/// Checks a packet that includes its transmitted checksum.
pub fn valid(bytes: &[u8]) -> bool {
    compute(bytes) == 0
}
