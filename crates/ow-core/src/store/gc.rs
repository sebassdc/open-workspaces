//! Controller-only operations, deliberately absent from runner `Objects`.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListedObject {
    pub key: String,
    pub bytes: u64,
    pub modified: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectVersion {
    pub bytes: u64,
    pub version: String,
}
/// Implementations must distinguish absence from authorization/transport failures.
pub trait ControllerObjects: Objects {
    fn configuration(&self) -> Result<()>;
    /// Unique write-once run marker; timestamp comes from the store's HEAD response.
    fn marker(&self, run: &str) -> Result<i64>;
    fn list(&self) -> Result<Vec<ListedObject>>;
    fn head(&self, key: &str) -> Result<Option<ObjectVersion>>;
    /// Writes a delete marker, never deletes a version. Returns its version ID.
    fn delete(&self, key: &str) -> Result<String>;
}

/// AWS CLI's UTC ISO-8601 timestamps, including fractional seconds; no local clock fallback.
pub(super) fn timestamp(s: &str) -> Result<i64> {
    let s = s
        .strip_suffix('Z')
        .or_else(|| s.strip_suffix("+00:00"))
        .context("S3 timestamp is not UTC")?;
    ensure!(s.len() >= 19 && s.is_ascii(), "invalid S3 timestamp");
    let b = s.as_bytes();
    ensure!(
        b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' && b[16] == b':',
        "invalid S3 timestamp"
    );
    let number =
        |a, z| -> Result<i64> { Ok(s.get(a..z).context("invalid S3 timestamp")?.parse()?) };
    let (mut y, m, d) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (h, min, sec) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    ensure!(
        (1970..=9999).contains(&y)
            && (1..=12).contains(&m)
            && (1..=days[(m - 1) as usize]).contains(&d)
            && (0..24).contains(&h)
            && (0..60).contains(&min)
            && (0..60).contains(&sec),
        "invalid S3 date"
    );
    ensure!(
        s.len() == 19
            || s[19..].starts_with('.')
                && s.len() > 20
                && s[20..].bytes().all(|b| b.is_ascii_digit()),
        "invalid S3 fraction"
    );
    y -= i64::from(m <= 2);
    let era = y / 400;
    let year = y - era * 400;
    let month = m + if m > 2 { -3 } else { 9 };
    let day = (153 * month + 2) / 5 + d - 1;
    Ok(
        (era * 146097 + year * 365 + year / 4 - year / 100 + day - 719468) * 86400
            + h * 3600
            + min * 60
            + sec,
    )
}

#[cfg(test)]
#[test]
fn s3_timestamps_have_no_local_age_fallback() {
    assert_eq!(timestamp("1970-01-01T00:00:00Z").unwrap(), 0);
    assert_eq!(
        timestamp("2026-10-19T12:34:56.123+00:00").unwrap(),
        1792413296
    );
    for bad in [
        "2026-02-30T00:00:00Z",
        "2026-01-01T24:00:00Z",
        "2026-01-01",
        "2026-01-01T00:00:00+01:00",
    ] {
        assert!(timestamp(bad).is_err());
    }
}
