//! Cryptographic primitives shared across backend implementations.
//!
//! RFC-023 Phase 2b.2.1: the Postgres + SQLite backends both need
//! Rust-side HMAC sign/verify for waitpoint tokens (the Valkey backend
//! signs inside Lua, so it does not consume this module). Extracted
//! from `ff-backend-postgres/src/signal.rs` with zero behaviour change
//! — same output bytes, same token wire shape, same error taxonomy.
//!
//! See [`hmac`] for the primitives.

pub mod hmac {
    //! HMAC-SHA256 sign/verify for `kid:hex` tokens.
    //!
    //! Token shape is `<kid>:<hex-digest>` where the digest is
    //! `HMAC-SHA256(secret, kid || ":" || message)`. Verification is
    //! constant-time via [`::hmac::Mac::verify_slice`].

    use ::hmac::{Hmac, Mac};
    use sha2::Sha256;

    /// HMAC-SHA256 signature over `kid || ":" || message`. Returns a
    /// `kid:hex` token.
    ///
    /// # Kid constraints
    ///
    /// The wire shape is `<kid>:<hex>`; a kid containing `':'` would
    /// collapse to an ambiguous `split_once(':')` at verify time. The
    /// backend seed / rotate entry points validate kid shape before
    /// minting secrets (backend-postgres `rotate_waitpoint_hmac_secret_all_impl`,
    /// backend-sqlite `suspend_ops::validate_kid`), so every secret
    /// reaching this function should already carry a colon-free kid.
    /// This primitive does not re-validate for performance — callers
    /// that build tokens from unvalidated input must run their own
    /// shape check first.
    pub fn hmac_sign(secret: &[u8], kid: &str, message: &[u8]) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
            .expect("HMAC-SHA256 accepts any key length");
        mac.update(kid.as_bytes());
        mac.update(b":");
        mac.update(message);
        let out = mac.finalize().into_bytes();
        format!("{kid}:{}", hex::encode(out))
    }

    /// Verify a `kid:hex` token. Returns `Ok(())` iff the digest
    /// matches `secret` over `message`. Constant-time comparison.
    pub fn hmac_verify(
        secret: &[u8],
        kid: &str,
        message: &[u8],
        token: &str,
    ) -> Result<(), HmacVerifyError> {
        let (tok_kid, tok_hex) =
            token.split_once(':').ok_or(HmacVerifyError::Malformed)?;
        if tok_kid != kid {
            return Err(HmacVerifyError::WrongKid {
                expected: kid.to_owned(),
                actual: tok_kid.to_owned(),
            });
        }
        let expected = hex::decode(tok_hex).map_err(|_| HmacVerifyError::Malformed)?;
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
            .map_err(|_| HmacVerifyError::Malformed)?;
        mac.update(kid.as_bytes());
        mac.update(b":");
        mac.update(message);
        mac.verify_slice(&expected)
            .map_err(|_| HmacVerifyError::SignatureMismatch)
    }

    /// Errors from [`hmac_verify`]. Backend callers map these onto
    /// `EngineError::Validation(InvalidInput)` at the trait boundary.
    #[derive(Debug, thiserror::Error)]
    pub enum HmacVerifyError {
        #[error("token malformed; expected kid:hex shape")]
        Malformed,
        #[error("token kid mismatch; expected {expected}, got {actual}")]
        WrongKid { expected: String, actual: String },
        #[error("HMAC signature mismatch")]
        SignatureMismatch,
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sign_then_verify_round_trip() {
            let secret = b"super-secret-key";
            let tok = hmac_sign(secret, "kid1", b"exec-id:wp-id");
            assert!(tok.starts_with("kid1:"));
            hmac_verify(secret, "kid1", b"exec-id:wp-id", &tok).expect("verify ok");
        }

        #[test]
        fn verify_rejects_tampered_message() {
            let secret = b"s";
            let tok = hmac_sign(secret, "k", b"msg");
            let err = hmac_verify(secret, "k", b"tampered", &tok).unwrap_err();
            assert!(matches!(err, HmacVerifyError::SignatureMismatch));
        }

        #[test]
        fn verify_rejects_wrong_kid() {
            let secret = b"s";
            let tok = hmac_sign(secret, "k1", b"msg");
            let err = hmac_verify(secret, "k2", b"msg", &tok).unwrap_err();
            assert!(matches!(err, HmacVerifyError::WrongKid { .. }));
        }

        #[test]
        fn verify_rejects_malformed() {
            assert!(matches!(
                hmac_verify(b"s", "k", b"msg", "no-colon-token"),
                Err(HmacVerifyError::Malformed)
            ));
            assert!(matches!(
                hmac_verify(b"s", "k", b"msg", "k:not-hex-zzzz"),
                Err(HmacVerifyError::Malformed)
            ));
        }

        #[test]
        fn sign_is_deterministic() {
            // Same inputs → same output bytes (no nonce / salt).
            let a = hmac_sign(b"k", "kid", b"msg");
            let b = hmac_sign(b"k", "kid", b"msg");
            assert_eq!(a, b);
        }
    }
}
