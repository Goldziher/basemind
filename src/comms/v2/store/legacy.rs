//! Setting the legacy store aside (ADR-0021, Storage): renamed, never deleted, never imported.

use std::path::{Path, PathBuf};

use super::records::backend;
use crate::comms::v2::api::CommsError;

/// Name of the v1 store's directory inside the comms directory.
pub const LEGACY_STORE_DIR: &str = "store.fjall";

/// Leading part of the name a retired legacy store gets; the date follows.
const RETIRED_PREFIX: &str = "store.legacy-";

/// Names tried for one date before giving up: the plain one, then `-2`, `-3` and so on.
const MAX_RETIRED_NAMES: u32 = 1000;

const MICROS_PER_DAY: i64 = 24 * 60 * 60 * 1_000_000;

/// Rename the v1 store in `comms_dir`, when there is one, to `store.legacy-<date>`, the UTC date
/// of `now_micros`. Returns where it went, or `None` when there was nothing to rename, so calling
/// it again is harmless.
///
/// Nothing is deleted or overwritten: when that name is taken, by an earlier retirement on the
/// same day, the directory becomes `store.legacy-<date>-2`, then `-3`. The caller must not have
/// the v1 store open, and must be the only one retiring in this directory: the name is checked
/// and then taken, which two callers at once could both pass.
pub fn retire_legacy_store(comms_dir: &Path, now_micros: i64) -> Result<Option<PathBuf>, CommsError> {
    let legacy = comms_dir.join(LEGACY_STORE_DIR);
    let exists = |path: &Path| {
        path.try_exists()
            .map_err(|error| backend(format_args!("look for {}", path.display()), error))
    };
    if !exists(&legacy)? {
        return Ok(None);
    }
    let dated = format!("{RETIRED_PREFIX}{}", utc_date(now_micros));
    for attempt in 1..=MAX_RETIRED_NAMES {
        let name = if attempt == 1 {
            dated.clone()
        } else {
            format!("{dated}-{attempt}")
        };
        let retired = comms_dir.join(name);
        if exists(&retired)? {
            continue;
        }
        std::fs::rename(&legacy, &retired).map_err(|error| {
            backend(
                format_args!("rename {} to {}", legacy.display(), retired.display()),
                error,
            )
        })?;
        return Ok(Some(retired));
    }
    Err(CommsError::Backend {
        detail: format!(
            "{} already holds {MAX_RETIRED_NAMES} legacy stores named {dated}*; the legacy store was left in place",
            comms_dir.display()
        ),
    })
}

/// `YYYY-MM-DD` of `now_micros` in UTC, by the days-to-civil algorithm of Howard Hinnant's
/// `chrono`-compatible date library.
fn utc_date(now_micros: i64) -> String {
    let days = now_micros.div_euclid(MICROS_PER_DAY) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
