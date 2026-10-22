//! Kubernetes-style resource quantities (`"500m"`, `"2"`, `"4Gi"`), which is
//! how the gateway expresses CPU and memory limits.

/// Parses a CPU quantity into millicores.
pub fn cpu_millis(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let invalid = || format!("invalid CPU quantity {value:?}");
    let millis = if let Some(millis) = value.strip_suffix('m') {
        millis.parse::<u64>().map_err(|_| invalid())?
    } else {
        let cores: f64 = value.parse().map_err(|_| invalid())?;
        if !cores.is_finite() || cores < 0.0 {
            return Err(invalid());
        }
        (cores * 1000.0).round() as u64
    };
    if millis == 0 {
        return Err(format!("CPU quantity {value:?} must be positive"));
    }
    Ok(millis)
}

/// Parses a memory quantity into bytes.
pub fn memory_bytes(value: &str) -> Result<u64, String> {
    let value = value.trim();
    let invalid = || format!("invalid memory quantity {value:?}");
    let split = value
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(value.len());
    let (number, suffix) = value.split_at(split);
    let number: f64 = number.parse().map_err(|_| invalid())?;
    let multiplier: f64 = match suffix {
        "" => 1.0,
        "k" => 1e3,
        "M" => 1e6,
        "G" => 1e9,
        "T" => 1e12,
        "P" => 1e15,
        "Ki" => 1024.0,
        "Mi" => 1024f64.powi(2),
        "Gi" => 1024f64.powi(3),
        "Ti" => 1024f64.powi(4),
        "Pi" => 1024f64.powi(5),
        _ => return Err(invalid()),
    };
    let bytes = (number * multiplier).round();
    if !bytes.is_finite() || bytes < 1.0 || bytes > u64::MAX as f64 {
        return Err(invalid());
    }
    Ok(bytes as u64)
}

/// LXD `limits.cpu.allowance` for a hard CPU cap, as quota per 100ms period.
pub fn lxd_cpu_allowance(millis: u64) -> String {
    format!("{}ms/100ms", (millis / 10).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu() {
        assert_eq!(cpu_millis("500m"), Ok(500));
        assert_eq!(cpu_millis("2"), Ok(2000));
        assert_eq!(cpu_millis("1.5"), Ok(1500));
        assert!(cpu_millis("0").is_err());
        assert!(cpu_millis("lots").is_err());
    }

    #[test]
    fn parses_memory() {
        assert_eq!(memory_bytes("4Gi"), Ok(4 * 1024 * 1024 * 1024));
        assert_eq!(memory_bytes("512Mi"), Ok(512 * 1024 * 1024));
        assert_eq!(memory_bytes("1G"), Ok(1_000_000_000));
        assert_eq!(memory_bytes("1048576"), Ok(1_048_576));
        assert!(memory_bytes("4GB").is_err());
        assert!(memory_bytes("").is_err());
    }

    #[test]
    fn converts_cpu_to_allowance() {
        assert_eq!(lxd_cpu_allowance(500), "50ms/100ms");
        assert_eq!(lxd_cpu_allowance(2000), "200ms/100ms");
        assert_eq!(lxd_cpu_allowance(1), "1ms/100ms");
    }
}
