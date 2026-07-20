//! The **weapi** request encryption scheme.
//!
//! NetEase's web player does not talk to its API in the clear: every request
//! body is a form with exactly two fields, `params` and `encSecKey`, produced by
//! the site's own JavaScript. Reimplementing it is the only way to call the API,
//! and the scheme is fixed in the wild — it cannot be negotiated, so all the
//! constants below are hardcoded on purpose.
//!
//! `params` is the JSON payload run through AES-128-CBC **twice**:
//!
//! 1. under a key baked into the JavaScript ([`PRESET_KEY`]), base64-encoded;
//! 2. under a random 16-character secret generated per request, base64 again.
//!
//! `encSecKey` is that random secret handed to the server, encrypted with
//! *textbook* RSA — no OAEP, no PKCS#1 v1.5, just `m^e mod n` over the reversed
//! secret left-zero-padded to 128 bytes. That is cryptographically weak, but it
//! is what the server expects; padding-enforcing RSA APIs cannot produce it,
//! which is why this uses a bare modular exponentiation.
//!
//! Both AES passes share one constant IV ([`IV`]) — again, fixed by the server.

use aes::cipher::{BlockEncryptMut as _, KeyIvInit as _, block_padding::Pkcs7};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use num_bigint::BigUint;
use rand::RngExt as _;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

/// Key of the first AES pass, lifted from NetEase's player JavaScript.
pub const PRESET_KEY: &[u8; 16] = b"0CoJUm6Qyw8W8jud";

/// The one IV both AES passes use. Reusing an IV is bad practice; the server
/// nonetheless requires exactly this one.
pub const IV: &[u8; 16] = b"0102030405060708";

/// RSA modulus (`n`), 1024-bit, hex.
pub const RSA_MODULUS_HEX: &str = "00e0b509f6259df8642dbc35662901477df22677ec152b5ff68ace615bb7b725152b3ab17a876aea8a5aa76d2e417629ec4ee341f56135fccf695280104e0312ecbda92557c93870114af6c9d05c4f7f0c3685b7a46bee255932575cce10b424d813cfe4875d3e82047b97ddef52741d546b8e289dc6935b3ece0462db0a22b8e7";

/// RSA public exponent (`e`), hex. 65537.
pub const RSA_EXPONENT_HEX: &str = "010001";

/// Length of the per-request secret. The RSA step depends on this being 16.
pub const SECRET_LEN: usize = 16;

/// Alphabet the random secret is drawn from — base62, matching the JavaScript.
const SECRET_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// An encrypted weapi request: the two form fields to POST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeapiRequest {
    /// Doubly AES-encrypted payload, base64.
    pub params: String,
    /// The per-request secret under textbook RSA, lowercase hex.
    pub enc_sec_key: String,
}

/// Encrypt `payload` (a JSON document) for the weapi endpoint, generating a
/// fresh random secret.
pub fn encrypt(payload: &str) -> WeapiRequest {
    encrypt_with_secret(payload, &random_secret())
}

/// Same as [`encrypt`], but with the per-request secret supplied by the caller.
///
/// Exposed so the scheme can be exercised deterministically in tests; production
/// callers want [`encrypt`], since reusing a secret across requests defeats the
/// (already thin) point of it.
///
/// # Panics
///
/// If `secret` is not exactly [`SECRET_LEN`] bytes.
pub fn encrypt_with_secret(payload: &str, secret: &[u8]) -> WeapiRequest {
    assert_eq!(
        secret.len(),
        SECRET_LEN,
        "the weapi secret must be exactly {SECRET_LEN} bytes"
    );
    let first = aes_cbc_base64(payload.as_bytes(), PRESET_KEY);
    WeapiRequest {
        params: aes_cbc_base64(first.as_bytes(), secret),
        enc_sec_key: rsa_encrypt(secret),
    }
}

/// A fresh 16-character base62 secret.
pub fn random_secret() -> Vec<u8> {
    let mut rng = rand::rng();
    (0..SECRET_LEN)
        .map(|_| SECRET_ALPHABET[rng.random_range(0..SECRET_ALPHABET.len())])
        .collect()
}

/// One AES-128-CBC pass with PKCS#7 padding and the fixed [`IV`], base64-encoded.
///
/// # Panics
///
/// If `key` is not 16 bytes.
fn aes_cbc_base64(data: &[u8], key: &[u8]) -> String {
    let cipher = Aes128CbcEnc::new_from_slices(key, IV).expect("AES-128 key and IV are 16 bytes");
    BASE64.encode(cipher.encrypt_padded_vec_mut::<Pkcs7>(data))
}

/// Textbook RSA over the **reversed** secret.
///
/// The reversal and the left zero-padding to the modulus width are both part of
/// the scheme: the JavaScript builds a big-endian integer out of the secret read
/// backwards, then emits the result as a fixed-width 256-character hex string.
fn rsa_encrypt(secret: &[u8]) -> String {
    let reversed: Vec<u8> = secret.iter().rev().copied().collect();
    let m = BigUint::from_bytes_be(&reversed);
    let n = BigUint::parse_bytes(RSA_MODULUS_HEX.as_bytes(), 16).expect("modulus is valid hex");
    let e = BigUint::parse_bytes(RSA_EXPONENT_HEX.as_bytes(), 16).expect("exponent is valid hex");
    let c = m.modpow(&e, &n);
    // `to_bytes_be` drops leading zero bytes; the server wants the full width.
    let bytes = c.to_bytes_be();
    let mut out = vec![0u8; 128 - bytes.len().min(128)];
    out.extend_from_slice(&bytes);
    hex::encode(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockDecryptMut as _;

    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

    /// A secret fixed here so every assertion below is reproducible. Its value
    /// is arbitrary — only its length matters to the scheme.
    const SECRET: &[u8; 16] = b"0123456789abcdef";

    fn aes_cbc_decrypt(base64: &str, key: &[u8]) -> Vec<u8> {
        let bytes = BASE64.decode(base64).unwrap();
        Aes128CbcDec::new_from_slices(key, IV)
            .unwrap()
            .decrypt_padded_vec_mut::<Pkcs7>(&bytes)
            .unwrap()
    }

    /// The whole point of the scheme is that the server can peel both layers
    /// back off, so peel them off here: outer layer under the per-request
    /// secret, inner layer under the preset key, and the payload comes back.
    #[test]
    fn both_aes_passes_round_trip() {
        let payload = r#"{"csrf_token":"","s":"晴天","type":"1"}"#;
        let req = encrypt_with_secret(payload, SECRET);

        let inner = aes_cbc_decrypt(&req.params, SECRET);
        let inner = String::from_utf8(inner).expect("inner layer is base64 text");
        let plain = aes_cbc_decrypt(&inner, PRESET_KEY);

        assert_eq!(String::from_utf8(plain).unwrap(), payload);
    }

    /// Nothing in the scheme is randomised once the secret is pinned; a request
    /// that varied run to run would mean state leaking in from somewhere.
    #[test]
    fn fixed_secret_gives_a_fixed_request() {
        let a = encrypt_with_secret("{}", SECRET);
        let b = encrypt_with_secret("{}", SECRET);
        assert_eq!(a, b);
    }

    /// The payload is the only input to `params`, and the secret the only input
    /// to `encSecKey` — neither may bleed into the other's output.
    #[test]
    fn payload_and_secret_are_independent_inputs() {
        let a = encrypt_with_secret("{}", SECRET);
        let b = encrypt_with_secret(r#"{"a":1}"#, SECRET);
        assert_eq!(a.enc_sec_key, b.enc_sec_key);
        assert_ne!(a.params, b.params);

        let c = encrypt_with_secret("{}", b"fedcba9876543210");
        assert_ne!(a.enc_sec_key, c.enc_sec_key);
        assert_ne!(a.params, c.params);
    }

    /// The server parses `encSecKey` as a fixed-width 1024-bit hex integer, so a
    /// short result (leading zero bytes stripped) would be rejected.
    #[test]
    fn enc_sec_key_is_128_bytes_of_lowercase_hex() {
        let req = encrypt_with_secret("{}", SECRET);
        assert_eq!(req.enc_sec_key.len(), 256);
        assert!(
            req.enc_sec_key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    /// Textbook RSA is deterministic, so `encSecKey` is a pure function of the
    /// secret — the same secret must always produce the same key, and this pins
    /// the reversal + padding convention against accidental "fixes".
    #[test]
    fn rsa_matches_a_manual_modpow_of_the_reversed_secret() {
        let n = BigUint::parse_bytes(RSA_MODULUS_HEX.as_bytes(), 16).unwrap();
        let e = BigUint::parse_bytes(RSA_EXPONENT_HEX.as_bytes(), 16).unwrap();
        let reversed: Vec<u8> = SECRET.iter().rev().copied().collect();
        let expected = BigUint::from_bytes_be(&reversed).modpow(&e, &n);

        let got = BigUint::parse_bytes(rsa_encrypt(SECRET).as_bytes(), 16).unwrap();
        assert_eq!(got, expected);
    }

    /// The secret feeds an AES-128 key directly, so a wrong length is a bug that
    /// must not reach the network.
    #[test]
    #[should_panic(expected = "must be exactly 16 bytes")]
    fn a_wrong_length_secret_is_rejected() {
        encrypt_with_secret("{}", b"too short");
    }

    #[test]
    fn random_secrets_are_base62_and_do_not_repeat() {
        let a = random_secret();
        assert_eq!(a.len(), SECRET_LEN);
        assert!(a.iter().all(|b| SECRET_ALPHABET.contains(b)));
        // 62^16 possibilities; a collision here means the RNG is not being used.
        assert_ne!(a, random_secret());
    }
}
