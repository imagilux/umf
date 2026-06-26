//! Small leaf helpers shared across the engine-build submodules.

use std::time::SystemTime;

/// Render the current wall-clock time as an RFC 3339 string for the
/// image-config `history[*].created` field.
///
/// Honours `SOURCE_DATE_EPOCH` (Reproducible Builds convention) so
/// downstream tests can pin the timestamp.
pub(crate) fn now_rfc3339() -> String {
    let epoch_secs = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        })
        .unwrap_or(0);
    // Convert epoch → civil time without pulling in chrono. Same shape
    // as the existing `civil_from_epoch` helper in stage.rs; copied
    // here to avoid pulling a builder-module cross-dep that would
    // bloat the engine_build entry point.
    let (year, month, day, hh, mm, ss) = civil_from_epoch(epoch_secs);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Days-from-epoch → (year, month, day, hh, mm, ss). Howard Hinnant's
/// `civil_from_days` algorithm — accurate across the full proleptic
/// Gregorian calendar.
const fn civil_from_epoch(epoch_secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let secs_per_day = 86_400_i64;
    let days = epoch_secs.div_euclid(secs_per_day);
    let day_secs = epoch_secs.rem_euclid(secs_per_day);
    let hh = (day_secs / 3600) as u32;
    let mm = ((day_secs % 3600) / 60) as u32;
    let ss = (day_secs % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5) + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d as u32, hh, mm, ss)
}
