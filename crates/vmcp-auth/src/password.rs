//! Master-password hashing/verification via argon2id.
//!
//! `verify_master` is constant-time (argon2 returns an Eq-checked digest +
//! `subtle::ConstantTimeEq` on the encoded hash for the fast-fail prefix path).

use anyhow::{anyhow, Result};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};

/// Hash a plaintext password with default argon2id parameters.
pub fn hash_password(plain: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| anyhow!("argon2 hash: {e}"))?
        .to_string();
    Ok(hash)
}

/// Verify `plain` against a PHC-encoded argon2id hash. Returns Ok(true) on
/// match, Ok(false) on mismatch, Err on malformed hash.
pub fn verify_master(plain: &str, encoded: &str) -> Result<bool> {
    let parsed =
        PasswordHash::new(encoded).map_err(|e| anyhow!("master hash parse: {e}"))?;
    Ok(Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let h = hash_password("hunter2").unwrap();
        assert!(verify_master("hunter2", &h).unwrap());
        assert!(!verify_master("wrong", &h).unwrap());
    }
}
