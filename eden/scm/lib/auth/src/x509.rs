/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::path::Path;
use std::path::PathBuf;

use anyhow::anyhow;
use chrono::DateTime;
use chrono::offset::Local;
use chrono::offset::Utc;
use simple_asn1::ASN1Block;
use thiserror::Error;

#[derive(Error, Debug)]
#[error("Problem with certificate {path}: {kind}")]
pub struct X509Error {
    #[source]
    pub kind: X509ErrorKind,
    pub path: PathBuf,
}

impl X509Error {
    fn new(kind: impl Into<X509ErrorKind>, path: impl AsRef<Path>) -> Self {
        Self {
            kind: kind.into(),
            path: path.as_ref().to_path_buf(),
        }
    }
}

#[derive(Error, Debug)]
pub enum X509ErrorKind {
    #[error("Certificate not found")]
    Missing(#[source] io::Error),
    #[error("Could not read certificate")]
    Unreadable(#[from] io::Error),
    #[error("Certificate is malformed")]
    Malformed(#[source] anyhow::Error),
    #[error(
        "Certificate has expired (valid from {0} to {1})",
        .not_before.with_timezone(&Local),
        .not_after.with_timezone(&Local))
    ]
    Expired {
        not_before: DateTime<Utc>,
        not_after: DateTime<Utc>,
    },
}

impl X509ErrorKind {
    pub fn is_missing(&self) -> bool {
        match self {
            Self::Missing(..) => true,
            _ => false,
        }
    }

    pub fn is_unreadable(&self) -> bool {
        match self {
            Self::Unreadable(..) => true,
            _ => false,
        }
    }

    pub fn is_malformed(&self) -> bool {
        match self {
            Self::Malformed(..) => true,
            _ => false,
        }
    }

    pub fn is_expired(&self) -> bool {
        match self {
            Self::Expired { .. } => true,
            _ => false,
        }
    }
}

/// Validate the dates of all X.509 certificates in the specified PEM file.
pub fn check_certs(path: impl AsRef<Path>) -> Result<(), X509Error> {
    let path = path.as_ref();
    let mut pem_file = File::open(path).map_err(|e| {
        let kind = match e.kind() {
            io::ErrorKind::NotFound => X509ErrorKind::Missing(e),
            _ => X509ErrorKind::Unreadable(e),
        };
        X509Error::new(kind, path)
    })?;
    let mut pem_bytes = Vec::new();
    pem_file
        .read_to_end(&mut pem_bytes)
        .map_err(|e| X509Error::new(e, path))?;

    certs_valid_at_time(&pem_bytes, Utc::now()).map_err(|e| X509Error::new(e, path))
}

/// Check whether all X.509 certificates found in the given PEM file would be
/// valid at a given time.
fn certs_valid_at_time(pem_bytes: &[u8], time: DateTime<Utc>) -> Result<(), X509ErrorKind> {
    let certs = pem::parse_many(pem_bytes)
        .unwrap()
        .into_iter()
        .filter(|pem| pem.tag() == "CERTIFICATE")
        .collect::<Vec<_>>();

    if certs.is_empty() {
        return Err(X509ErrorKind::Malformed(anyhow!(
            "PEM file does not contain an X.509 certificate"
        )));
    }

    for cert in certs {
        cert_is_valid_at(cert.contents(), time)?;
    }

    Ok(())
}

/// Check whether an X.509 certificate would be valid at a given time.
///
/// This function only checks that the given time falls within the certificate's
/// valid date range. It does not perform any cryptographic verification of the
/// certificate's signature.
///
/// The input is expected to be a DER-encoded binary certificate that conforms
/// to the ASN.1 schema for X.509 certificates specified in [RFC 5280](RFC5280).
///
/// [RFC5280]: https://tools.ietf.org/html/rfc5280#section-4.1
fn cert_is_valid_at(cert: &[u8], time: DateTime<Utc>) -> Result<(), X509ErrorKind> {
    let (not_before, not_after) = parse_valid_date_range(cert)?;

    if not_before < time && time < not_after {
        Ok(())
    } else {
        Err(X509ErrorKind::Expired {
            not_before,
            not_after,
        })
    }
}

/// Parse and extract the valid date range from a DER-encoded X.509 certificate.
fn parse_valid_date_range(cert: &[u8]) -> Result<(DateTime<Utc>, DateTime<Utc>), X509ErrorKind> {
    let asn1 = simple_asn1::from_der(cert).map_err(|e| X509ErrorKind::Malformed(e.into()))?;

    // XXX: Due to some technical issues with vendoring 3rd party dependencies,
    // we presently can't use the `x509-parser` crate that would make this much
    // nicer. Instead, we have to parse the DER-encoded ASN.1 object manually.
    // The field indexes that are hardcoded here correspond to the field order
    // specified in RFC 5280, which all X.509 certificates must conform to.
    if let Some(ASN1Block::Sequence(_, cert)) = asn1.first() {
        if let Some(ASN1Block::Sequence(_, fields)) = cert.first() {
            if let Some(ASN1Block::Sequence(_, validity)) = fields.get(4) {
                if let Some(ASN1Block::UTCTime(_, not_before)) = validity.first() {
                    if let Some(ASN1Block::UTCTime(_, not_after)) = validity.get(1) {
                        let not_before = DateTime::from_timestamp(
                            not_before.assume_utc().unix_timestamp(),
                            not_before.nanosecond(),
                        );
                        let not_after = DateTime::from_timestamp(
                            not_after.assume_utc().unix_timestamp(),
                            not_after.nanosecond(),
                        );
                        if let (Some(not_before), Some(not_after)) = (not_before, not_after) {
                            return Ok((not_before, not_after));
                        }
                    }
                }
            }
        }
    }

    Err(X509ErrorKind::Malformed(anyhow!(
        "Certificate does not contain expected fields"
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use anyhow::Result;
    use chrono::offset::TimeZone;

    use super::*;

    const CERT_1: &[u8] = include_bytes!("test_certs/cert1.pem");
    const CERT_2: &[u8] = include_bytes!("test_certs/cert2.pem");
    const NOT_A_CERT: &[u8] = include_bytes!("test_certs/not_a_cert.pem");
    const COMBINED: &[u8] = include_bytes!("test_certs/combined.pem");

    // CERT_1 is valid from 2020-12-09 22:39:13 UTC to 2020-12-10 22:39:13 UTC.
    static CERT_1_NOT_BEFORE: LazyLock<DateTime<Utc>> =
        LazyLock::new(|| Utc.with_ymd_and_hms(2020, 12, 9, 22, 39, 13).unwrap());
    static CERT_1_NOT_AFTER: LazyLock<DateTime<Utc>> =
        LazyLock::new(|| Utc.with_ymd_and_hms(2020, 12, 10, 22, 39, 13).unwrap());

    // CERT_2 is valid from  2020-12-09 22:40:23 UTC to 2020-12-11 22:40:23 UTC.
    static CERT_2_NOT_BEFORE: LazyLock<DateTime<Utc>> =
        LazyLock::new(|| Utc.with_ymd_and_hms(2020, 12, 9, 22, 40, 23).unwrap());
    static CERT_2_NOT_AFTER: LazyLock<DateTime<Utc>> =
        LazyLock::new(|| Utc.with_ymd_and_hms(2020, 12, 11, 22, 40, 23).unwrap());

    // Both CERT_1 and CERT_1 are valid on this date.
    static CERT_1_VALID_DATE: LazyLock<DateTime<Utc>> =
        LazyLock::new(|| Utc.with_ymd_and_hms(2020, 12, 10, 0, 0, 0).unwrap());

    // On this date, CERT_2 is valid but CERT_1 is not.
    static CERT_2_VALID_DATE: LazyLock<DateTime<Utc>> =
        LazyLock::new(|| Utc.with_ymd_and_hms(2020, 12, 11, 0, 0, 0).unwrap());

    #[test]
    fn test_date_parsing() -> Result<()> {
        let pem = pem::parse(CERT_1)?;
        let (not_before, not_after) = parse_valid_date_range(pem.contents())?;
        assert_eq!(not_before, *CERT_1_NOT_BEFORE);
        assert_eq!(not_after, *CERT_1_NOT_AFTER);

        let pem = pem::parse(CERT_2)?;
        let (not_before, not_after) = parse_valid_date_range(pem.contents())?;
        assert_eq!(not_before, *CERT_2_NOT_BEFORE);
        assert_eq!(not_after, *CERT_2_NOT_AFTER);

        Ok(())
    }

    #[test]
    fn test_single_cert() -> Result<()> {
        certs_valid_at_time(CERT_1, *CERT_1_VALID_DATE)?;
        let res = certs_valid_at_time(CERT_1, *CERT_2_VALID_DATE);
        assert!(res.unwrap_err().is_expired());

        certs_valid_at_time(CERT_2, *CERT_1_VALID_DATE)?;
        certs_valid_at_time(CERT_2, *CERT_2_VALID_DATE)?;

        Ok(())
    }

    #[test]
    fn test_combined_certs() -> Result<()> {
        // The input file contains both CERT_1 and CERT_2. Since CERT_1 is
        // invalid on CERT_2_VALID_DATE, the entire file is considered invalid
        // on that date because it contains an expired cert.
        certs_valid_at_time(COMBINED, *CERT_1_VALID_DATE)?;
        let res = certs_valid_at_time(COMBINED, *CERT_2_VALID_DATE);
        assert!(res.unwrap_err().is_expired());

        Ok(())
    }

    #[test]
    fn test_no_cert() -> Result<()> {
        // The input file is a valid PEM file, but does not contain a cert.
        let res = certs_valid_at_time(NOT_A_CERT, *CERT_1_VALID_DATE);
        assert!(res.unwrap_err().is_malformed());

        Ok(())
    }
}
