use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use rand::RngCore;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed")]
    Decrypt,
    #[error("nonce length must be 12 bytes")]
    BadNonce,
}

pub struct TokenCipher {
    cipher: Aes256Gcm,
}

impl TokenCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new(key.into()),
        }
    }

    pub fn encrypt(&self, plaintext: &str) -> Result<(Vec<u8>, [u8; 12]), CryptoError> {
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_bytes())
            .map_err(|_| CryptoError::Encrypt)?;
        Ok((ciphertext, nonce_bytes))
    }

    pub fn decrypt(&self, ciphertext: &[u8], nonce: &[u8]) -> Result<String, CryptoError> {
        if nonce.len() != 12 {
            return Err(CryptoError::BadNonce);
        }
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| CryptoError::Decrypt)?;
        String::from_utf8(plaintext).map_err(|_| CryptoError::Decrypt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = [7u8; 32];
        let c = TokenCipher::new(&key);
        let (ct, nonce) = c.encrypt("access-production-deadbeef").unwrap();
        assert_ne!(ct.as_slice(), b"access-production-deadbeef");
        let pt = c.decrypt(&ct, &nonce).unwrap();
        assert_eq!(pt, "access-production-deadbeef");
    }

    #[test]
    fn wrong_key_fails() {
        let (ct, nonce) = TokenCipher::new(&[1u8; 32]).encrypt("secret").unwrap();
        assert!(TokenCipher::new(&[2u8; 32]).decrypt(&ct, &nonce).is_err());
    }
}
