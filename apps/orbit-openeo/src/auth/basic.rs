//! HTTP Basic auth — base64 username:password extraction + compare.

use thiserror::Error;

/// Parsed `Authorization: Basic …` header.
#[derive(Debug, Clone)]
pub struct BasicCredentials {
    username: String,
    password: String,
}

/// Parse error for the Basic scheme.
#[derive(Debug, Error)]
pub enum BasicError {
    /// Header value didn't start with "Basic ".
    #[error("not a Basic Authorization header")]
    NotBasic,
    /// Base64 payload couldn't be decoded.
    #[error("invalid base64 payload")]
    BadBase64,
    /// Decoded payload didn't contain a `:` separator.
    #[error("missing colon in credentials")]
    MissingColon,
    /// Decoded payload wasn't valid UTF-8.
    #[error("non-utf8 credentials")]
    NotUtf8,
}

impl BasicCredentials {
    /// Parse `Basic <base64(user:pass)>`.
    pub fn parse(header: &str) -> Result<Self, BasicError> {
        let payload = header.strip_prefix("Basic ").ok_or(BasicError::NotBasic)?;
        let decoded = b64_decode(payload).ok_or(BasicError::BadBase64)?;
        let s = std::str::from_utf8(&decoded).map_err(|_| BasicError::NotUtf8)?;
        let (u, p) = s.split_once(':').ok_or(BasicError::MissingColon)?;
        Ok(Self { username: u.to_string(), password: p.to_string() })
    }

    /// Constant-time check against expected (user, pass).
    #[must_use]
    pub fn matches(&self, expected_user: &str, expected_pass: &str) -> bool {
        constant_time_eq(self.username.as_bytes(), expected_user.as_bytes())
            & constant_time_eq(self.password.as_bytes(), expected_pass.as_bytes())
    }

    /// Decoded username.
    #[must_use]
    pub fn username(&self) -> &str { &self.username }
    /// Decoded password.
    #[must_use]
    pub fn password(&self) -> &str { &self.password }
}

/// Standard base64 decode (table from RFC 4648). Tiny hand-rolled impl so
/// we don't pull `base64` for one call site.
fn b64_decode(input: &str) -> Option<Vec<u8>> {
    const T: [i8; 256] = {
        let mut t = [-1i8; 256];
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut i = 0;
        while i < alphabet.len() {
            t[alphabet[i] as usize] = i as i8;
            i += 1;
        }
        t[b'=' as usize] = 0; // padding tolerated
        t
    };
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = [0u8; 4];
    let mut len = 0;
    let mut pad = 0;
    for &b in bytes {
        if b == b'\n' || b == b'\r' || b == b' ' { continue; }
        let v = T[b as usize];
        if v < 0 { return None; }
        buf[len] = v as u8;
        if b == b'=' { pad += 1; }
        len += 1;
        if len == 4 {
            let triple = ((buf[0] as u32) << 18)
                | ((buf[1] as u32) << 12)
                | ((buf[2] as u32) << 6)
                | (buf[3] as u32);
            out.push(((triple >> 16) & 0xff) as u8);
            if pad < 2 {
                out.push(((triple >> 8) & 0xff) as u8);
            }
            if pad < 1 {
                out.push((triple & 0xff) as u8);
            }
            len = 0;
            pad = 0;
        }
    }
    if len != 0 { return None; }
    Some(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() { return false; }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_alice_wonder() {
        // base64("alice:wonder") = "YWxpY2U6d29uZGVy"
        let c = BasicCredentials::parse("Basic YWxpY2U6d29uZGVy").unwrap();
        assert_eq!(c.username(), "alice");
        assert_eq!(c.password(), "wonder");
    }

    #[test]
    fn matches_correct() {
        let c = BasicCredentials::parse("Basic YWxpY2U6d29uZGVy").unwrap();
        assert!(c.matches("alice", "wonder"));
        assert!(!c.matches("alice", "wrong"));
        assert!(!c.matches("bob", "wonder"));
    }

    #[test]
    fn parse_rejects_bearer() {
        assert!(matches!(
            BasicCredentials::parse("Bearer abc"),
            Err(BasicError::NotBasic)
        ));
    }

    #[test]
    fn parse_rejects_bad_base64() {
        assert!(matches!(
            BasicCredentials::parse("Basic !!!not-b64!!!"),
            Err(BasicError::BadBase64)
        ));
    }

    #[test]
    fn parse_rejects_missing_colon() {
        // base64("nocolon") = "bm9jb2xvbg=="
        assert!(matches!(
            BasicCredentials::parse("Basic bm9jb2xvbg=="),
            Err(BasicError::MissingColon)
        ));
    }

    #[test]
    fn parse_handles_padding() {
        // base64("a:b") = "YTpi"
        let c = BasicCredentials::parse("Basic YTpi").unwrap();
        assert_eq!(c.username(), "a");
        assert_eq!(c.password(), "b");
    }

    #[test]
    fn b64_decode_known_vector() {
        // "Hello, World!" → "SGVsbG8sIFdvcmxkIQ=="
        assert_eq!(b64_decode("SGVsbG8sIFdvcmxkIQ==").unwrap(), b"Hello, World!");
    }
}
