/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::LazyLock;

use chrono::FixedOffset;
use chrono::TimeZone;
use mononoke_types::DateTime;

/// Return a `DateTime` corresponding to `<year>-01-01 00:00:00` UTC.
pub fn day_1_utc(year: i32) -> DateTime {
    DateTime::new(
        FixedOffset::west_opt(0)
            .unwrap()
            .with_ymd_and_hms(year, 1, 1, 0, 0, 0)
            .unwrap(),
    )
}

/// Return a `DateTime` corresponding to `<year>-01-01 00:00:00` UTC,
/// with the specified offset applied.
pub fn day_1_tz(year: i32, offset: i32) -> DateTime {
    DateTime::new(
        FixedOffset::west_opt(offset)
            .unwrap()
            .with_ymd_and_hms(year, 1, 1, 0, 0, 0)
            .unwrap(),
    )
}

pub const PST_OFFSET: i32 = 7 * 3600;

/// 1970-01-01 00:00:00 UTC.
pub static EPOCH_ZERO: LazyLock<DateTime> =
    LazyLock::new(|| DateTime::from_timestamp(0, 0).unwrap());
/// 1970-01-01 00:00:00 UTC-07.
pub static EPOCH_ZERO_PST: LazyLock<DateTime> =
    LazyLock::new(|| DateTime::from_timestamp(0, PST_OFFSET).unwrap());

/// 1900-01-01 00:00:00 UTC.
pub static YEAR_1900: LazyLock<DateTime> = LazyLock::new(|| day_1_utc(1900));
/// 1900-01-01 00:00:00 UTC-07.
pub static YEAR_1900_PST: LazyLock<DateTime> = LazyLock::new(|| day_1_tz(1900, PST_OFFSET));

/// 2000-01-01 00:00:00 UTC.
pub static YEAR_2000: LazyLock<DateTime> = LazyLock::new(|| day_1_utc(2000));
/// 2000-01-01 00:00:00 UTC-07.
pub static YEAR_2000_PST: LazyLock<DateTime> = LazyLock::new(|| day_1_tz(2000, PST_OFFSET));

/// 2100-01-01 00:00:00 UTC.
pub static YEAR_2100: LazyLock<DateTime> = LazyLock::new(|| day_1_utc(2100));
pub static YEAR_2100_PST: LazyLock<DateTime> = LazyLock::new(|| day_1_tz(2100, PST_OFFSET));
