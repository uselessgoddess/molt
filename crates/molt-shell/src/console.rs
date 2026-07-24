//! Where a shell's output goes.

/// A byte sink the shell prints through.
///
/// Bytes rather than [`core::fmt::Write`], because `cat` prints whatever a file
/// holds and a file is not obliged to hold UTF-8. A port that can fail has
/// nowhere to report it — a shell cannot un-print a line — so writing is
/// infallible and a failing device drops output instead of unwinding.
pub trait Console {
    /// Writes every byte of `bytes`.
    fn write(&mut self, bytes: &[u8]);

    /// Writes `bytes` and ends the line.
    fn line(&mut self, bytes: &[u8]) {
        self.write(bytes);
        self.write(b"\n");
    }

    /// Writes `value` in decimal.
    fn number(&mut self, value: u64) {
        // 20 digits is `u64::MAX`, so the loop never runs off the end.
        let mut digits = [0u8; 20];
        let mut written = 0;
        let mut left = value;
        loop {
            digits[digits.len() - 1 - written] = b'0' + (left % 10) as u8;
            written += 1;
            left /= 10;
            if left == 0 {
                break;
            }
        }
        self.write(&digits[digits.len() - written..]);
    }
}

impl<C: Console + ?Sized> Console for &mut C {
    fn write(&mut self, bytes: &[u8]) {
        (**self).write(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::Console;
    use crate::capture::Capture;

    #[test]
    fn numbers_print_in_decimal() {
        let mut out = Capture::new();
        out.number(0);
        out.write(b" ");
        out.number(1024);
        out.write(b" ");
        out.number(u64::MAX);

        assert_eq!(out.text(), "0 1024 18446744073709551615");
    }
}
