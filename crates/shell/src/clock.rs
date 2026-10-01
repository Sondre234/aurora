//! Clock formatting and the minute-aligned schedule. Pure apart from [`local_now`].

/// Broken-down local time, only what the bar prints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tm {
    /// 0 = Sunday.
    pub wday: u32,
    pub mday: u32,
    /// 0 = January.
    pub mon: u32,
    pub hour: u32,
    pub min: u32,
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `Tue 30 Sep  14:05`.
pub fn format_clock(t: &Tm) -> String {
    format!(
        "{} {} {}  {:02}:{:02}",
        DAYS[t.wday as usize % 7],
        t.mday,
        MONTHS[t.mon as usize % 12],
        t.hour % 24,
        t.min % 60
    )
}

/// Milliseconds from `epoch_ms` to the next minute boundary, in `1..=60_000`.
pub fn ms_until_next_minute(epoch_ms: u64) -> u64 {
    60_000 - epoch_ms % 60_000
}

/// Milliseconds since the Unix epoch.
pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// The current local time (honors `TZ` and /etc/localtime through libc).
pub fn local_now() -> Tm {
    let secs = (epoch_ms() / 1000) as libc::time_t;
    // SAFETY: `tm` is plain data that localtime_r fully initializes on success, and both
    // pointers are valid for the call.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return Tm {
                wday: 0,
                mday: 1,
                mon: 0,
                hour: 0,
                min: 0,
            };
        }
        tm
    };
    Tm {
        wday: tm.tm_wday.max(0) as u32,
        mday: tm.tm_mday.max(1) as u32,
        mon: tm.tm_mon.max(0) as u32,
        hour: tm.tm_hour.max(0) as u32,
        min: tm.tm_min.max(0) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_wraps_out_of_range_fields() {
        let t = Tm {
            wday: 2,
            mday: 30,
            mon: 8,
            hour: 14,
            min: 5,
        };
        assert_eq!(format_clock(&t), "Tue 30 Sep  14:05");
        let t = Tm {
            wday: 9,
            mday: 1,
            mon: 13,
            hour: 0,
            min: 0,
        };
        assert_eq!(format_clock(&t), "Tue 1 Feb  00:00");
    }

    #[test]
    fn aligns_to_the_minute() {
        assert_eq!(ms_until_next_minute(0), 60_000);
        assert_eq!(ms_until_next_minute(1), 59_999);
        assert_eq!(ms_until_next_minute(59_999), 1);
        assert_eq!(ms_until_next_minute(120_000 + 30_250), 29_750);
    }
}
