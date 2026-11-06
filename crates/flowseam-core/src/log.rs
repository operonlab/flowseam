//! Timestamped worker diagnostics written directly to the process stderr.
use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn format_at(ms: u64, message: &str) -> String {
    let day = ms % 86_400_000;
    format!("{:02}:{:02}:{:02}.{:03}Z {message}", day / 3_600_000, day / 60_000 % 60, day / 1000 % 60, day % 1000)
}

pub fn line(message: &str) {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let _ = writeln!(io::stderr().lock(), "{}", format_at(ms, message));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_utc_timestamp() {
        assert_eq!(format_at(86_400_000 + 3_723_004, "up"), "01:02:03.004Z up");
    }
}
