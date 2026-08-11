use anyhow::{Context, Result};
use time::{Date, Month, PrimitiveDateTime, Time};

use super::FileVersionIndex;

pub(crate) fn effective_gallery_captured_at_unix(
    key: &str,
    metadata_extracted: bool,
    taken_at_unix: Option<u64>,
    version_created_at_unix: Option<u64>,
) -> u64 {
    if let Some(taken_at_unix) = taken_at_unix {
        return taken_at_unix;
    }
    if metadata_extracted && let Some(filename_captured_at_unix) = filename_captured_at_unix(key) {
        return filename_captured_at_unix;
    }
    version_created_at_unix.unwrap_or(0)
}

pub(super) fn version_created_at_unix(
    index: &FileVersionIndex,
    manifest_hash: &str,
) -> Option<u64> {
    index
        .versions
        .values()
        .filter(|record| record.manifest_hash == manifest_hash)
        .map(|record| record.created_at_unix)
        .max()
}

pub(crate) fn version_created_at_unix_from_payload(
    payload: Option<&[u8]>,
    manifest_hash: &str,
) -> Result<Option<u64>> {
    let Some(payload) = payload else {
        return Ok(None);
    };
    let index = serde_json::from_slice::<FileVersionIndex>(payload)
        .context("invalid version index while resolving gallery capture time")?;
    Ok(version_created_at_unix(&index, manifest_hash))
}

fn filename_captured_at_unix(key: &str) -> Option<u64> {
    let filename = key.rsplit('/').next().unwrap_or(key);
    let stem = filename
        .rsplit_once('.')
        .map(|(stem, _extension)| stem)
        .unwrap_or(filename);
    let bytes = stem.as_bytes();

    (0..bytes.len()).find_map(|start| {
        if !bytes[start].is_ascii_digit()
            || start
                .checked_sub(1)
                .and_then(|index| bytes.get(index))
                .is_some_and(u8::is_ascii_digit)
        {
            return None;
        }

        parse_compact_date(bytes, start)
            .or_else(|| parse_separated_date(bytes, start))
            .and_then(|(date, end)| {
                let time = parse_time_after_date(bytes, end).unwrap_or(Time::MIDNIGHT);
                u64::try_from(
                    PrimitiveDateTime::new(date, time)
                        .assume_utc()
                        .unix_timestamp(),
                )
                .ok()
            })
    })
}

fn parse_compact_date(bytes: &[u8], start: usize) -> Option<(Date, usize)> {
    let year = parse_digits(bytes, start, 4)?;
    let month = parse_digits(bytes, start + 4, 2)?;
    let day = parse_digits(bytes, start + 6, 2)?;
    Some((calendar_date(year, month, day)?, start + 8))
}

fn parse_separated_date(bytes: &[u8], start: usize) -> Option<(Date, usize)> {
    let first_separator = *bytes.get(start + 4)?;
    let second_separator = *bytes.get(start + 7)?;
    if !is_date_separator(first_separator) || !is_date_separator(second_separator) {
        return None;
    }

    let year = parse_digits(bytes, start, 4)?;
    let month = parse_digits(bytes, start + 5, 2)?;
    let day = parse_digits(bytes, start + 8, 2)?;
    Some((calendar_date(year, month, day)?, start + 10))
}

fn calendar_date(year: u32, month: u32, day: u32) -> Option<Date> {
    let year = i32::try_from(year).ok()?;
    let month = Month::try_from(u8::try_from(month).ok()?).ok()?;
    Date::from_calendar_date(year, month, u8::try_from(day).ok()?).ok()
}

fn parse_time_after_date(bytes: &[u8], date_end: usize) -> Option<Time> {
    if bytes.get(date_end).is_some_and(u8::is_ascii_digit) {
        return parse_compact_time(bytes, date_end);
    }

    let mut start = skip_date_time_separators(bytes, date_end);
    if bytes
        .get(start..start + 2)
        .is_some_and(|value| value.eq_ignore_ascii_case(b"at"))
    {
        start = skip_date_time_separators(bytes, start + 2);
    }

    parse_compact_time(bytes, start).or_else(|| parse_separated_time(bytes, start))
}

fn parse_compact_time(bytes: &[u8], start: usize) -> Option<Time> {
    let hour = parse_digits(bytes, start, 2)?;
    let minute = parse_digits(bytes, start + 2, 2)?;
    let second = parse_digits(bytes, start + 4, 2)?;
    Time::from_hms(
        u8::try_from(hour).ok()?,
        u8::try_from(minute).ok()?,
        u8::try_from(second).ok()?,
    )
    .ok()
}

fn parse_separated_time(bytes: &[u8], start: usize) -> Option<Time> {
    if !is_time_separator(*bytes.get(start + 2)?) || !is_time_separator(*bytes.get(start + 5)?) {
        return None;
    }

    let hour = parse_digits(bytes, start, 2)?;
    let minute = parse_digits(bytes, start + 3, 2)?;
    let second = parse_digits(bytes, start + 6, 2)?;
    Time::from_hms(
        u8::try_from(hour).ok()?,
        u8::try_from(minute).ok()?,
        u8::try_from(second).ok()?,
    )
    .ok()
}

fn parse_digits(bytes: &[u8], start: usize, length: usize) -> Option<u32> {
    let digits = bytes.get(start..start + length)?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    digits.iter().try_fold(0u32, |value, digit| {
        value.checked_mul(10)?.checked_add(u32::from(*digit - b'0'))
    })
}

fn skip_date_time_separators(bytes: &[u8], mut start: usize) -> usize {
    while bytes
        .get(start)
        .is_some_and(|value| matches!(value, b' ' | b'_' | b'-' | b'.' | b'T' | b't'))
    {
        start += 1;
    }
    start
}

fn is_date_separator(value: u8) -> bool {
    matches!(value, b' ' | b'_' | b'-' | b'.')
}

fn is_time_separator(value: u8) -> bool {
    matches!(value, b'_' | b'-' | b'.' | b':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_camera_screenshot_and_messaging_filenames() {
        let cases = [
            ("IMG_20240304_050607.jpg", (2024, 3, 4, 5, 6, 7)),
            ("PXL_20240304_050607123.jpg", (2024, 3, 4, 5, 6, 7)),
            ("Screenshot 2024-03-04 050607.png", (2024, 3, 4, 5, 6, 7)),
            (
                "Screenshot 2024-03-04 at 05.06.07.png",
                (2024, 3, 4, 5, 6, 7),
            ),
            ("IMG-20240304-WA0001.jpg", (2024, 3, 4, 0, 0, 0)),
        ];

        for (filename, expected) in cases {
            assert_eq!(
                filename_captured_at_unix(filename),
                Some(unix_timestamp(expected)),
                "unexpected timestamp for {filename}"
            );
        }
    }

    #[test]
    fn rejects_invalid_or_embedded_numeric_dates() {
        assert_eq!(filename_captured_at_unix("IMG_20241340_999999.jpg"), None);
        assert_eq!(
            filename_captured_at_unix("received_120240304_050607.jpg"),
            None
        );
        assert_eq!(filename_captured_at_unix("holiday.jpg"), None);
    }

    #[test]
    fn capture_time_fallbacks_follow_metadata_lifecycle() {
        let filename_time = filename_captured_at_unix("IMG_20240304_050607.jpg").unwrap();

        assert_eq!(
            effective_gallery_captured_at_unix("IMG_20240304_050607.jpg", false, None, Some(200),),
            200,
            "pending metadata must use the version creation time"
        );
        assert_eq!(
            effective_gallery_captured_at_unix(
                "IMG_20240304_050607.jpg",
                true,
                Some(300),
                Some(200),
            ),
            300,
            "extracted capture time must take precedence"
        );
        assert_eq!(
            effective_gallery_captured_at_unix("IMG_20240304_050607.jpg", true, None, Some(200),),
            filename_time,
            "filename time must replace the creation fallback once extraction completes"
        );
        assert_eq!(
            effective_gallery_captured_at_unix("holiday.jpg", true, None, Some(200)),
            200,
            "unrecognized filenames must retain the version creation fallback"
        );
    }

    fn unix_timestamp((year, month, day, hour, minute, second): (i32, u8, u8, u8, u8, u8)) -> u64 {
        let date = Date::from_calendar_date(year, Month::try_from(month).unwrap(), day).unwrap();
        let time = Time::from_hms(hour, minute, second).unwrap();
        u64::try_from(
            PrimitiveDateTime::new(date, time)
                .assume_utc()
                .unix_timestamp(),
        )
        .unwrap()
    }
}
