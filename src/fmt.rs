//! Human-friendly numbers. Sizes use SI units, like drive labels and dd itself.

pub fn bytes(n: u64) -> String {
    if n < 1000 {
        return format!("{n} B");
    }
    let mut value = n as f64;
    let mut unit = "B";
    for next in ["kB", "MB", "GB", "TB", "PB"] {
        if value < 999.5 {
            break;
        }
        value /= 1000.0;
        unit = next;
    }
    format!("{} {unit}", three_digits(value))
}

pub fn speed(bytes_per_sec: f64) -> String {
    format!("{}/s", bytes(bytes_per_sec.max(0.0) as u64))
}

pub fn duration(secs: f64) -> String {
    if secs > 0.0 && secs < 0.5 {
        return "<1s".to_owned();
    }
    let secs = secs.max(0.0).round() as u64;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs / 60 % 60),
    }
}

/// dd's notation: 4194304 → "4M".
pub fn block_size(n: u64) -> String {
    for (unit, size) in [("G", 1u64 << 30), ("M", 1 << 20), ("K", 1 << 10)] {
        if n >= size && n.is_multiple_of(size) {
            return format!("{}{unit}", n / size);
        }
    }
    n.to_string()
}

fn three_digits(v: f64) -> String {
    if v < 9.995 {
        format!("{v:.2}")
    } else if v < 99.95 {
        format!("{v:.1}")
    } else {
        format!("{v:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(5_910_000_000), "5.91 GB");
        assert_eq!(bytes(31_914_983_424), "31.9 GB");
        assert_eq!(bytes(256_060_514_304), "256 GB");
        assert_eq!(bytes(999_999), "1.00 MB");
        assert_eq!(block_size(4 << 20), "4M");
        assert_eq!(block_size(512 << 10), "512K");
        assert_eq!(block_size(1000), "1000");
        assert_eq!(duration(42.0), "42s");
        assert_eq!(duration(78.0), "1m 18s");
        assert_eq!(duration(3900.0), "1h 05m");
    }
}
