//! Local passwords: argon2id hashes, checked off the async runtime (hashing is deliberately slow).

use anyhow::{Result, anyhow};
use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

pub const MIN_LEN: usize = 12;
const MAX_LEN: usize = 256;

/// Why a new password is not acceptable, if it isn't.
pub fn weakness(password: &str, email: &str) -> Option<String> {
    let n = password.chars().count();
    if n < MIN_LEN {
        return Some(format!("use at least {MIN_LEN} characters"));
    }
    if n > MAX_LEN {
        return Some(format!("use at most {MAX_LEN} characters"));
    }
    let lower = password.to_lowercase();
    let local = email.split('@').next().unwrap_or("").to_lowercase();
    if lower == email.to_lowercase() || (local.len() >= 3 && lower.contains(&local)) {
        return Some("don't use your email address in the password".into());
    }
    let first = password.chars().next().unwrap_or(' ');
    if password.chars().all(|c| c == first) {
        return Some("use more than one distinct character".into());
    }
    None
}

pub async fn hash(password: String) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| anyhow!("hashing password: {e}"))
    })
    .await?
}

/// Check `password` against `stored`. With no stored hash (unknown user or SSO-only), a dummy
/// hash is checked anyway so the answer takes about as long either way.
pub async fn verify(password: String, stored: Option<String>) -> Result<bool> {
    tokio::task::spawn_blocking(move || {
        let real = stored.is_some();
        let stored = stored.unwrap_or_else(|| DUMMY.to_string());
        let parsed = PasswordHash::new(&stored).map_err(|e| anyhow!("stored hash: {e}"))?;
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        Ok(ok && real)
    })
    .await?
}

/// argon2id of a random string nobody knows.
const DUMMY: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$K5ZMEqDA0hLBV4LxPFkg5XPb4eEFmTl8mVQoKhHXo8s";

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashes_verify_and_differ() {
        let a = hash("correct horse battery".into()).await.unwrap();
        let b = hash("correct horse battery".into()).await.unwrap();
        assert_ne!(a, b, "salted");
        assert!(a.starts_with("$argon2id$"));
        assert!(
            verify("correct horse battery".into(), Some(a.clone()))
                .await
                .unwrap()
        );
        assert!(!verify("wrong horse battery".into(), Some(a)).await.unwrap());
        assert!(!verify("anything at all".into(), None).await.unwrap());
    }

    #[test]
    fn weak_passwords_are_refused() {
        assert!(weakness("short", "a@b.co").is_some());
        assert!(weakness("aaaaaaaaaaaaaaa", "a@b.co").is_some());
        assert!(weakness("ana.lopez-2026!", "ana.lopez@example.com").is_some());
        assert!(weakness("a much longer passphrase", "ana@example.com").is_none());
    }
}
