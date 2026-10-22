//! AWS Signature Version 4, as S3 and S3-compatible services use it.
//!
//! <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub struct Credentials<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
}

/// What's signed of a request.
pub struct Request<'a> {
    pub method: &'a str,
    /// The path, already URI-encoded.
    pub path: &'a str,
    /// Unencoded; encoded and sorted when signing.
    pub query: &'a [(&'a str, &'a str)],
    /// Every header to sign, which must include `host` and `x-amz-date`.
    pub headers: &'a [(String, String)],
    /// Hex SHA-256 of the body, or `UNSIGNED-PAYLOAD`.
    pub payload_hash: &'a str,
}

/// The `authorization` header for `request`, made at `amz_date`
/// (`YYYYMMDDTHHMMSSZ`, the request's `x-amz-date`).
pub fn authorization(
    request: &Request,
    credentials: &Credentials,
    region: &str,
    service: &str,
    amz_date: &str,
) -> String {
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), canonical_value(value)))
        .collect();
    headers.sort();
    let signed_headers = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();

    let mut query: Vec<(String, String)> = request
        .query
        .iter()
        .map(|(name, value)| (encode(name, true), encode(value, true)))
        .collect();
    query.sort();
    let canonical_query = query
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&");

    let canonical_request = format!(
        "{}\n{}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{}",
        request.method, request.path, request.payload_hash
    );

    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );

    let key = format!("AWS4{}", credentials.secret_access_key);
    let key = hmac(key.as_bytes(), date.as_bytes());
    let key = hmac(&key, region.as_bytes());
    let key = hmac(&key, service.as_bytes());
    let key = hmac(&key, b"aws4_request");
    let signature = hex(&hmac(&key, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    )
}

/// URI-encodes `text` as SigV4 expects: everything but unreserved
/// characters, and `/` too unless `slash` is false (for paths).
pub fn encode(text: &str, slash: bool) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            b'/' if !slash => encoded.push('/'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// `YYYYMMDDTHHMMSSZ` for `seconds` since the Unix epoch.
pub fn amz_date(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let time = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes keys of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Header values are trimmed, with runs of spaces collapsed.
fn canonical_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The date `days` after 1970-01-01, by Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREDENTIALS: Credentials = Credentials {
        access_key_id: "AKIDEXAMPLE",
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
    };

    /// `get-vanilla` from AWS's SigV4 test suite.
    #[test]
    fn signs_the_vanilla_request() {
        let headers = [
            ("Host".to_owned(), "example.amazonaws.com".to_owned()),
            ("X-Amz-Date".to_owned(), "20150830T123600Z".to_owned()),
        ];
        let request = Request {
            method: "GET",
            path: "/",
            query: &[],
            headers: &headers,
            payload_hash: &sha256_hex(b""),
        };
        assert_eq!(
            authorization(
                &request,
                &CREDENTIALS,
                "us-east-1",
                "service",
                "20150830T123600Z"
            ),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// `get-vanilla-query-order-key-case` from AWS's SigV4 test suite.
    #[test]
    fn sorts_the_query() {
        let headers = [
            ("Host".to_owned(), "example.amazonaws.com".to_owned()),
            ("X-Amz-Date".to_owned(), "20150830T123600Z".to_owned()),
        ];
        let request = Request {
            method: "GET",
            path: "/",
            query: &[("Param2", "value2"), ("Param1", "value1")],
            headers: &headers,
            payload_hash: &sha256_hex(b""),
        };
        assert!(
            authorization(
                &request,
                &CREDENTIALS,
                "us-east-1",
                "service",
                "20150830T123600Z"
            )
            .ends_with(
                "Signature=b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
            )
        );
    }

    #[test]
    fn encodes_paths_and_values() {
        assert_eq!(encode("a b/c~d.JPG", false), "a%20b/c~d.JPG");
        assert_eq!(encode("a b/c", true), "a%20b%2Fc");
        assert_eq!(encode("é", true), "%C3%A9");
    }

    #[test]
    fn formats_dates() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(amz_date(1_440_938_160), "20150830T123600Z");
        assert_eq!(amz_date(1_709_210_096), "20240229T123456Z");
    }
}
