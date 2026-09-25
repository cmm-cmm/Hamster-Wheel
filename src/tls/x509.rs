//! The few X.509 facts TLS v1 checks at startup: validity dates and whether a
//! certificate is self-issued.
//!
//! rustls and webpki verify signatures and paths but do not expose a
//! certificate's dates, and a leaf whose issuer is not in the file cannot be
//! path-verified at all. This is a minimal DER walk over the fixed prefix of
//! `TBSCertificate` (RFC 5280 section 4.1): version, serial, signature
//! algorithm, issuer, validity, subject. It never interprets extensions and
//! rejects anything it does not recognise instead of guessing.

use std::fmt;

const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
const EXPLICIT_VERSION: u8 = 0xa0;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;

/// Seconds since the Unix epoch, UTC. Signed: `notBefore` may predate 1970.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Timestamp(pub(crate) i64);

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0.div_euclid(86_400);
        let seconds = self.0.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        write!(
            formatter,
            "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
            seconds / 3_600,
            seconds % 3_600 / 60,
            seconds % 60
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) not_before: Timestamp,
    pub(crate) not_after: Timestamp,
    /// Issuer and subject names are byte-identical. A self-signed root or a
    /// self-signed leaf; its issuer cannot be missing from a chain.
    pub(crate) self_issued: bool,
}

pub(crate) fn summarize(der: &[u8]) -> Result<Summary, &'static str> {
    let mut certificate = Reader(der).element(SEQUENCE)?;
    let mut tbs = Reader(certificate.element(SEQUENCE)?.0);
    if tbs.peek() == Some(EXPLICIT_VERSION) {
        tbs.element(EXPLICIT_VERSION)?;
    }
    tbs.element(INTEGER)?;
    tbs.element(SEQUENCE)?;
    let issuer = tbs.element(SEQUENCE)?;
    let mut validity = Reader(tbs.element(SEQUENCE)?.0);
    let not_before = validity.time()?;
    let not_after = validity.time()?;
    let subject = tbs.element(SEQUENCE)?;
    Ok(Summary {
        not_before,
        not_after,
        self_issued: issuer.0 == subject.0,
    })
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn peek(&self) -> Option<u8> {
        self.0.first().copied()
    }

    /// Consumes one element with the expected tag and returns its contents.
    fn element(&mut self, tag: u8) -> Result<Reader<'a>, &'static str> {
        let (&found, rest) = self.0.split_first().ok_or("truncated DER")?;
        if found != tag {
            return Err("unexpected DER structure");
        }
        let (&first, mut rest) = rest.split_first().ok_or("truncated DER")?;
        let length = if first < 0x80 {
            usize::from(first)
        } else {
            let count = usize::from(first & 0x7f);
            if count == 0 || count > 4 || rest.len() < count {
                return Err("unsupported DER length");
            }
            let length = rest[..count]
                .iter()
                .fold(0_usize, |length, &byte| (length << 8) | usize::from(byte));
            rest = &rest[count..];
            length
        };
        if rest.len() < length {
            return Err("truncated DER");
        }
        let (contents, remaining) = rest.split_at(length);
        self.0 = remaining;
        Ok(Reader(contents))
    }

    fn time(&mut self) -> Result<Timestamp, &'static str> {
        match self.peek() {
            Some(UTC_TIME) => parse_time(self.element(UTC_TIME)?.0, false),
            Some(GENERALIZED_TIME) => parse_time(self.element(GENERALIZED_TIME)?.0, true),
            _ => Err("validity is not a UTCTime or GeneralizedTime"),
        }
    }
}

/// RFC 5280 4.1.2.5: `YYMMDDHHMMSSZ` (years 1950-2049) or `YYYYMMDDHHMMSSZ`,
/// always UTC with seconds and no fraction.
fn parse_time(text: &[u8], generalized: bool) -> Result<Timestamp, &'static str> {
    let digits = if generalized { 14 } else { 12 };
    if text.len() != digits + 1
        || text[digits] != b'Z'
        || !text[..digits].iter().all(u8::is_ascii_digit)
    {
        return Err("validity time is not in RFC 5280 form");
    }
    let number = |range: std::ops::Range<usize>| {
        text[range]
            .iter()
            .fold(0_i64, |value, &digit| value * 10 + i64::from(digit - b'0'))
    };
    let (year, rest) = if generalized {
        (number(0..4), 4)
    } else {
        let short = number(0..2);
        (
            if short < 50 {
                2_000 + short
            } else {
                1_900 + short
            },
            2,
        )
    };
    let month = number(rest..rest + 2);
    let day = number(rest + 2..rest + 4);
    let hour = number(rest + 4..rest + 6);
    let minute = number(rest + 6..rest + 8);
    let second = number(rest + 8..rest + 10);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err("validity time is out of range");
    }
    let days = days_from_civil(year, month, day);
    Ok(Timestamp(
        days * 86_400 + hour * 3_600 + minute * 60 + second,
    ))
}

/// Howard Hinnant's `days_from_civil`: proleptic Gregorian date to days since
/// 1970-01-01.
const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn certificate(not_before: (i32, u8, u8), not_after: (i32, u8, u8)) -> Vec<u8> {
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(not_before.0, not_before.1, not_before.2);
        params.not_after = rcgen::date_time_ymd(not_after.0, not_after.1, not_after.2);
        let key = rcgen::KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().der().to_vec()
    }

    #[test]
    fn reads_utc_and_generalized_validity_times() {
        // rcgen writes UTCTime before 2050 and GeneralizedTime from 2050.
        let summary = summarize(&certificate((1999, 12, 31), (2050, 1, 2))).unwrap();
        assert_eq!(summary.not_before.to_string(), "1999-12-31 00:00:00 UTC");
        assert_eq!(summary.not_after.to_string(), "2050-01-02 00:00:00 UTC");
        assert!(summary.self_issued);
        assert_eq!(
            Timestamp(days_from_civil(1970, 1, 1) * 86_400),
            Timestamp(0)
        );
        assert_eq!(
            summarize(&certificate((2024, 2, 29), (2024, 3, 1)))
                .unwrap()
                .not_after
                .0
                - summarize(&certificate((2024, 2, 29), (2024, 3, 1)))
                    .unwrap()
                    .not_before
                    .0,
            86_400,
            "a leap day is one day"
        );
    }

    #[test]
    fn a_certificate_signed_by_another_is_not_self_issued() {
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(rcgen::DnType::CommonName, "Hamster Test CA");
        let issuer =
            rcgen::CertifiedIssuer::self_signed(ca, rcgen::KeyPair::generate().unwrap()).unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
            .unwrap()
            .signed_by(&rcgen::KeyPair::generate().unwrap(), &issuer)
            .unwrap();
        assert!(!summarize(leaf.der()).unwrap().self_issued);
        assert!(summarize(issuer.der()).unwrap().self_issued);
    }

    #[test]
    fn rejects_malformed_der_and_times() {
        let good = certificate((2020, 1, 1), (2030, 1, 1));
        for length in [0, 1, 2, 10, good.len() / 2] {
            assert!(summarize(&good[..length]).is_err(), "{length}");
        }
        assert!(summarize(b"not DER").is_err());
        assert!(parse_time(b"2001011200Z", false).is_err());
        assert!(parse_time(b"200101120000+0100", false).is_err());
        assert!(parse_time(b"201301120000Z", false).is_err());
        assert!(parse_time(b"20200101120000.5Z", true).is_err());
        assert_eq!(
            parse_time(b"491231235959Z", false).unwrap().to_string(),
            "2049-12-31 23:59:59 UTC"
        );
        assert_eq!(
            parse_time(b"500101000000Z", false).unwrap().to_string(),
            "1950-01-01 00:00:00 UTC"
        );
    }
}
