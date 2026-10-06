/// Convert dB to raw volume (0..=161)
pub fn db_to_raw(db: f64) -> i32 {
    ((db + 80.5) * 2.0).round() as i32
}

/// Convert raw volume to dB
pub fn raw_to_db(raw: i32) -> f64 {
    (raw as f64) * 0.5 - 80.5
}

/// Clamp raw volume to the range [0, max_raw]
pub fn clamp_raw(raw: i32, cap_db: f64) -> i32 {
    let max_raw = db_to_raw(cap_db);
    raw.clamp(0, max_raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_db_to_raw() {
        assert_eq!(db_to_raw(-80.5), 0);
        assert_eq!(db_to_raw(0.0), 161);
        assert_eq!(db_to_raw(-33.0), 95);
        assert_eq!(db_to_raw(-15.0), 131);
        assert_eq!(db_to_raw(-5.0), 151);
    }

    #[test]
    fn test_raw_to_db() {
        assert_eq!(raw_to_db(0), -80.5);
        assert_eq!(raw_to_db(161), 0.0);
        assert_eq!(raw_to_db(95), -33.0);
        assert_eq!(raw_to_db(131), -15.0);
        assert_eq!(raw_to_db(151), -5.0);
    }

    #[test]
    fn test_clamp_raw_main_zone() {
        // main zone: cap -15.0 dB = raw 131
        assert_eq!(clamp_raw(200, -15.0), 131);
        assert_eq!(clamp_raw(131, -15.0), 131);
        assert_eq!(clamp_raw(100, -15.0), 100);
        assert_eq!(clamp_raw(-10, -15.0), 0);
    }

    #[test]
    fn test_clamp_raw_zone2() {
        // zone2: cap 0.0 dB = raw 161
        assert_eq!(clamp_raw(200, 0.0), 161);
        assert_eq!(clamp_raw(161, 0.0), 161);
        assert_eq!(clamp_raw(100, 0.0), 100);
    }
}
