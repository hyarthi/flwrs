//! Which request headers reach an event: never secrets, never hop-by-hop
//! headers; lowercase names, values as UTF-8 text.

use axum::http::HeaderMap;
use std::collections::{BTreeMap, BTreeSet};

/// Left out unless an allowlist names them.
pub const DEFAULT_DENYLIST: [&str; 5] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
];

/// Meaningful for one hop only (RFC 9110 §7.6.1); always left out.
pub const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderPolicy {
    /// When set, only these (and never hop-by-hop ones).
    allow: Option<BTreeSet<String>>,
    deny: BTreeSet<String>,
}

/// The headers an event carries, and how many values were left out for not
/// being UTF-8.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filtered {
    pub headers: BTreeMap<String, Vec<String>>,
    pub non_utf8: usize,
}

impl HeaderPolicy {
    /// `secret_headers`: names the auth settings use (e.g. an API key's
    /// header), always denied unless allowed explicitly.
    pub fn new(
        allow: Option<Vec<String>>,
        deny: Option<Vec<String>>,
        secret_headers: &[String],
    ) -> Self {
        let lower = |v: Vec<String>| {
            v.into_iter()
                .map(|h| h.to_ascii_lowercase())
                .collect::<BTreeSet<_>>()
        };
        let mut denied = deny
            .map(lower)
            .unwrap_or_else(|| DEFAULT_DENYLIST.iter().map(|h| h.to_string()).collect());
        denied.extend(secret_headers.iter().map(|h| h.to_ascii_lowercase()));
        Self {
            allow: allow.map(lower),
            deny: denied,
        }
    }

    pub fn passes(&self, name: &str) -> bool {
        if HOP_BY_HOP.contains(&name) {
            return false;
        }
        match &self.allow {
            Some(allow) => allow.contains(name),
            None => !self.deny.contains(name),
        }
    }

    pub fn filter(&self, headers: &HeaderMap) -> Filtered {
        let mut out = Filtered::default();
        for (name, value) in headers {
            if !self.passes(name.as_str()) {
                continue;
            }
            match value.to_str() {
                Ok(v) => out
                    .headers
                    .entry(name.as_str().to_string())
                    .or_default()
                    .push(v.to_string()),
                Err(_) => out.non_utf8 += 1,
            }
        }
        out
    }
}

impl Default for HeaderPolicy {
    fn default() -> Self {
        Self::new(None, None, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("Authorization", HeaderValue::from_static("Bearer x"));
        h.insert("Connection", HeaderValue::from_static("keep-alive"));
        h.insert("X-Token", HeaderValue::from_static("t"));
        h.append("Accept", HeaderValue::from_static("a"));
        h.append("Accept", HeaderValue::from_static("b"));
        h.insert("X-Bin", HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap());
        h
    }

    #[test]
    fn the_default_policy_leaves_out_secrets_and_hop_by_hop() {
        let f = HeaderPolicy::new(None, None, &["X-Token".into()]).filter(&headers());
        let names: Vec<&str> = f.headers.keys().map(String::as_str).collect();
        assert_eq!(names, ["accept"]);
        assert_eq!(f.headers["accept"], ["a", "b"]);
        assert_eq!(f.non_utf8, 1);
    }

    #[test]
    fn an_allowlist_wins_but_never_passes_hop_by_hop() {
        let p = HeaderPolicy::new(
            Some(vec!["Authorization".into(), "connection".into()]),
            None,
            &[],
        );
        let f = p.filter(&headers());
        let names: Vec<&str> = f.headers.keys().map(String::as_str).collect();
        assert_eq!(names, ["authorization"]);
        let p = HeaderPolicy::new(None, Some(vec!["accept".into()]), &[]);
        assert!(p.passes("authorization") && !p.passes("accept"));
    }
}
