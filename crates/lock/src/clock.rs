//! Clock text. The formatting is pure; `now_local` reads the wall clock through libc.

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Broken-down local time, the fields of `struct tm` the lock screen needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 = Sunday.
    pub weekday: u32,
    pub day: u32,
    /// 0 = January.
    pub month: u32,
}

impl LocalTime {
    /// `14:05`
    pub fn clock(&self) -> String {
        format!("{:02}:{:02}", self.hour % 24, self.minute % 60)
    }

    /// `Wednesday, 30 September`
    pub fn date(&self) -> String {
        format!(
            "{}, {} {}",
            DAYS[(self.weekday % 7) as usize],
            self.day,
            MONTHS[(self.month % 12) as usize]
        )
    }

    /// Milliseconds until the minute changes, at least 1.
    pub fn ms_to_next_minute(&self) -> u64 {
        (60 - u64::from(self.second % 60)) * 1000
    }
}

/// The current local time, or `None` if libc cannot convert it.
pub fn now_local() -> Option<LocalTime> {
    // SAFETY: `time` with a null pointer only returns the value.
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    // SAFETY: a zeroed `tm` is a valid out parameter for `localtime_r`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are live locals.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some(LocalTime {
        hour: tm.tm_hour.max(0) as u32,
        minute: tm.tm_min.max(0) as u32,
        second: tm.tm_sec.max(0) as u32,
        weekday: tm.tm_wday.max(0) as u32,
        day: tm.tm_mday.max(1) as u32,
        month: tm.tm_mon.max(0) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u32, minute: u32, second: u32) -> LocalTime {
        LocalTime {
            hour,
            minute,
            second,
            weekday: 3,
            day: 30,
            month: 8,
        }
    }

    #[test]
    fn clock_is_zero_padded_24h() {
        assert_eq!(at(14, 5, 0).clock(), "14:05");
        assert_eq!(at(0, 0, 0).clock(), "00:00");
        assert_eq!(at(9, 59, 59).clock(), "09:59");
    }

    #[test]
    fn date_reads_naturally() {
        assert_eq!(at(0, 0, 0).date(), "Wednesday, 30 September");
    }

    #[test]
    fn next_minute_wait() {
        assert_eq!(at(1, 1, 0).ms_to_next_minute(), 60_000);
        assert_eq!(at(1, 1, 59).ms_to_next_minute(), 1_000);
        // A leap second must not underflow.
        assert_eq!(at(1, 1, 60).ms_to_next_minute(), 60_000);
    }
}
