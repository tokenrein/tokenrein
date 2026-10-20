//! Encryption for provider API keys saved from the dashboard.
//!
//! Keys are sealed with AES-256-GCM under a 32-byte secret from the environment
//! (`TL_SECRETS_KEY` by default), so a copy of Redis alone does not reveal them. The org and
//! provider are bound in as associated data, so a sealed key cannot be moved to another org.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Result, anyhow, bail};

const NONCE_LEN: usize = 12;

pub struct SecretBox(Aes256Gcm);

impl SecretBox {
    /// `hex` is 64 hex characters (32 bytes), e.g. from `openssl rand -hex 32`.
    pub fn from_hex(hex_key: &str) -> Result<Self> {
        let bytes = hex::decode(hex_key.trim())
            .map_err(|_| anyhow!("secrets key must be 64 hex characters"))?;
        if bytes.len() != 32 {
            bail!("secrets key must be 64 hex characters (32 bytes)");
        }
        Ok(Self(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&bytes))))
    }

    /// Hex of nonce || ciphertext.
    pub fn seal(&self, context: &str, plaintext: &str) -> Result<String> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .0
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| anyhow!("encryption failed"))?;
        let mut out = nonce.to_vec();
        out.extend(ct);
        Ok(hex::encode(out))
    }

    pub fn open(&self, context: &str, sealed: &str) -> Result<String> {
        let bytes = hex::decode(sealed).map_err(|_| anyhow!("sealed value is not hex"))?;
        if bytes.len() <= NONCE_LEN {
            bail!("sealed value is too short");
        }
        let (nonce, ct) = bytes.split_at(NONCE_LEN);
        let pt = self
            .0
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ct,
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| {
                anyhow!(
                    "cannot decrypt: the secrets key is different from the one it was saved with"
                )
            })?;
        String::from_utf8(pt).map_err(|_| anyhow!("decrypted value is not UTF-8"))
    }
}

/// The associated data for an org's provider key.
pub fn context(org: &str, provider: &str) -> String {
    format!("tokenlens/provider-key/{org}/{provider}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const K1: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const K2: &str = "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100";

    #[test]
    fn round_trips_and_is_bound_to_key_and_context() {
        let b = SecretBox::from_hex(K1).unwrap();
        let sealed = b
            .seal(&context("acme", "anthropic"), "sk-ant-secret")
            .unwrap();
        assert!(!sealed.contains("secret"));
        assert_eq!(
            b.open(&context("acme", "anthropic"), &sealed).unwrap(),
            "sk-ant-secret"
        );
        // Another org, another provider or another secrets key cannot open it.
        assert!(b.open(&context("other", "anthropic"), &sealed).is_err());
        assert!(b.open(&context("acme", "openai"), &sealed).is_err());
        let other = SecretBox::from_hex(K2).unwrap();
        assert!(other.open(&context("acme", "anthropic"), &sealed).is_err());
        // Fresh nonce every time.
        assert_ne!(
            sealed,
            b.seal(&context("acme", "anthropic"), "sk-ant-secret")
                .unwrap()
        );
    }

    #[test]
    fn rejects_bad_secrets_keys() {
        assert!(SecretBox::from_hex("short").is_err());
        assert!(SecretBox::from_hex(&"zz".repeat(32)).is_err());
        assert!(SecretBox::from_hex(&"ab".repeat(16)).is_err());
    }
}
