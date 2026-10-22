//! AWS Signature Version 4, for the one warehouse that needs it. The
//! algorithm is short and fixed — a canonical request hashed into a
//! string to sign, an HMAC chain from the secret through the date, the
//! region and the service — and `ring` has the HMAC and the SHA-256, so
//! it is written here rather than brought in with an SDK. The test holds
//! the worked example from the signing documentation.

use crate::Secret;

pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: Secret,
    pub session_token: Option<Secret>,
}

/// A request to sign: the headers it already carries, which must include
/// `host`, and the body.
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// The query string without its `?`, already encoded.
    pub query: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

/// The headers that make the request signed: `x-amz-date`, `authorization`
/// and, with a session token, `x-amz-security-token`. `now` is
/// `YYYYMMDDTHHMMSSZ`.
pub fn sign(creds: &Credentials, region: &str, service: &str, req: &Request<'_>, now: &str) -> Vec<(String, String)> {
    let date = &now[..8];
    // Names lower-cased, values trimmed with runs of spaces collapsed, as
    // the canonical form asks.
    let mut headers: Vec<(String, String)> =
        req.headers.iter().map(|(k, v)| (k.to_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" "))).collect();
    headers.push(("x-amz-date".into(), now.to_string()));
    if let Some(t) = &creds.session_token {
        headers.push(("x-amz-security-token".into(), t.expose().to_string()));
    }
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers: String = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        req.method,
        if req.path.is_empty() { "/" } else { req.path },
        req.query,
        hex(sha256(req.body))
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{now}\n{scope}\n{}", hex(sha256(canonical.as_bytes())));
    let k_date = hmac(format!("AWS4{}", creds.secret_access_key.expose()).as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(hmac(&k_signing, to_sign.as_bytes()));
    let authorization =
        format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", creds.access_key_id);
    let mut out = vec![("x-amz-date".to_string(), now.to_string()), ("authorization".to_string(), authorization)];
    if let Some(t) = &creds.session_token {
        out.push(("x-amz-security-token".into(), t.expose().to_string()));
    }
    out
}

/// The time as a signature wants it, from the system clock.
pub fn now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    stamp(secs)
}

/// `YYYYMMDDTHHMMSSZ` for a unix time.
pub fn stamp(secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil_time(secs);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// Year, month, day, hour, minute, second of a unix time, by the civil
/// date arithmetic of Howard Hinnant's `days_from_civil` inverse.
pub(crate) fn civil_time(secs: u64) -> (i64, u32, u32, u64, u64, u64) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn sha256(data: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().to_vec()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), data).as_ref().to_vec()
}

fn hex(bytes: Vec<u8>) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
