//! The session cookie: claims sealed with ChaCha20-Poly1305 (`ring`), under a
//! key derived from the secret key with HKDF-SHA256. The cookie can't be read
//! or changed without the secret key, and a new secret key makes every
//! existing cookie fail to open (everyone is logged out).
//!
//! Cookie value: base64url (no padding) of `nonce (12 bytes) || ciphertext ||
//! tag (16 bytes)`; the plaintext is the claims as JSON. Nonces are random
//! (96 bits), which is safe for any realistic number of logins.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::hkdf;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

const SALT: &[u8] = b"flwrs session cookie";
const INFO: &[u8] = b"session v1";
/// Bound to the format version, so a later format can't be confused with it.
const AAD: &[u8] = b"flwrs_session.v1";
const VERSION: u8 = 1;
const TAG_LEN: usize = 16;
/// Cookies longer than this aren't even decoded.
const MAX_COOKIE_LEN: usize = 4096;

/// What a session says about its holder.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct Claims {
    v: u8,
    /// How the holder logged in (`key`; later `db`, `oidc`).
    pub provider: String,
    /// Who (`admin` for the key).
    pub subject: String,
    /// Not used yet: a slot for permissions.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Unix seconds.
    pub issue_time: i64,
    /// Unix seconds; the session is refused from then on.
    pub expire_time: i64,
}

impl Claims {
    pub(crate) fn new(provider: &str, subject: &str, issue_time: i64, expire_time: i64) -> Self {
        Self {
            v: VERSION,
            provider: provider.to_string(),
            subject: subject.to_string(),
            roles: Vec::new(),
            issue_time,
            expire_time,
        }
    }
}

pub(crate) struct Sealer {
    key: LessSafeKey,
    rng: SystemRandom,
}

impl Sealer {
    pub(crate) fn new(secret: &[u8]) -> Self {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, SALT).extract(secret);
        let okm = prk
            .expand(&[INFO], &CHACHA20_POLY1305)
            .expect("HKDF output of one key length");
        Self {
            key: LessSafeKey::new(UnboundKey::from(okm)),
            rng: SystemRandom::new(),
        }
    }

    pub(crate) fn seal(&self, claims: &Claims) -> Result<String, String> {
        let mut nonce = [0u8; NONCE_LEN];
        self.rng
            .fill(&mut nonce)
            .map_err(|_| "no randomness for a session nonce".to_string())?;
        let mut data = serde_json::to_vec(claims).map_err(|e| e.to_string())?;
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(AAD),
                &mut data,
            )
            .map_err(|_| "sealing the session failed".to_string())?;
        let mut out = Vec::with_capacity(NONCE_LEN + data.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&data);
        Ok(URL_SAFE_NO_PAD.encode(out))
    }

    /// The claims, if the cookie was sealed by this key and is well formed
    /// (expiry is the caller's check).
    pub(crate) fn open(&self, cookie: &str) -> Option<Claims> {
        if cookie.len() > MAX_COOKIE_LEN {
            return None;
        }
        let raw = URL_SAFE_NO_PAD.decode(cookie).ok()?;
        if raw.len() < NONCE_LEN + TAG_LEN {
            return None;
        }
        let (nonce, sealed) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let mut sealed = sealed.to_vec();
        let plain = self
            .key
            .open_in_place(nonce, Aad::from(AAD), &mut sealed)
            .ok()?;
        let claims: Claims = serde_json::from_slice(plain).ok()?;
        (claims.v == VERSION).then_some(claims)
    }
}
