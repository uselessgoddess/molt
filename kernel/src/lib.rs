#[macro_export]
macro_rules! report {
    ($platform:expr, $($arg:tt)*) => {{
        use core::fmt::Write as _;

        let _ = writeln!(SerialWriter::new($platform.serial()), $($arg)*);
    }};
}
