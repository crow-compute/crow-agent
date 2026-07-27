use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead as _, KeyInit as _, Payload},
};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore as _};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, Zeroizing};

const WRAP_INFO: &[u8] = b"crow-agent-bundle-key-wrap-v1";

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("invalid encoded cryptographic field")]
    Encoding,
    #[error("cryptographic operation failed")]
    Operation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleCiphertext {
    pub nonce: String,
    pub ciphertext: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedBundleKey {
    pub ephemeral_public_key: String,
    pub nonce: String,
    pub wrapped_key: String,
}

pub struct DeviceEncryptionKey {
    secret: StaticSecret,
}

impl std::fmt::Debug for DeviceEncryptionKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceEncryptionKey")
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

impl DeviceEncryptionKey {
    #[must_use]
    pub fn generate() -> Self {
        Self {
            secret: StaticSecret::random_from_rng(OsRng),
        }
    }

    #[must_use]
    pub fn from_secret(secret: [u8; 32]) -> Self {
        Self {
            secret: StaticSecret::from(secret),
        }
    }

    #[must_use]
    pub fn secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.secret.to_bytes())
    }

    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        PublicKey::from(&self.secret).to_bytes()
    }

    pub fn unwrap_bundle_key(
        &self,
        wrapped: &WrappedBundleKey,
        aad: &[u8],
    ) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
        let ephemeral = decode_32(&wrapped.ephemeral_public_key)?;
        let shared = self.secret.diffie_hellman(&PublicKey::from(ephemeral));
        let wrapping_key = derive_wrapping_key(shared.as_bytes(), aad)?;
        let nonce = decode_24(&wrapped.nonce)?;
        let ciphertext = URL_SAFE_NO_PAD
            .decode(&wrapped.wrapped_key)
            .map_err(|_| CryptoError::Encoding)?;
        let plaintext = XChaCha20Poly1305::new((&*wrapping_key).into())
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad,
                },
            )
            .map_err(|_| CryptoError::Operation)?;
        let array: [u8; 32] = plaintext.try_into().map_err(|_| CryptoError::Operation)?;
        Ok(Zeroizing::new(array))
    }
}

#[must_use]
pub fn generate_bundle_key() -> Zeroizing<[u8; 32]> {
    let mut key = Zeroizing::new([0_u8; 32]);
    OsRng.fill_bytes(&mut *key);
    key
}

pub fn wrap_bundle_key(
    bundle_key: &[u8; 32],
    recipient_public_key: &[u8; 32],
    aad: &[u8],
) -> Result<WrappedBundleKey, CryptoError> {
    let ephemeral_secret = StaticSecret::random_from_rng(OsRng);
    let ephemeral_public = PublicKey::from(&ephemeral_secret);
    let shared = ephemeral_secret.diffie_hellman(&PublicKey::from(*recipient_public_key));
    let wrapping_key = derive_wrapping_key(shared.as_bytes(), aad)?;
    let mut nonce = [0_u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let wrapped = XChaCha20Poly1305::new((&*wrapping_key).into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: bundle_key,
                aad,
            },
        )
        .map_err(|_| CryptoError::Operation)?;
    Ok(WrappedBundleKey {
        ephemeral_public_key: URL_SAFE_NO_PAD.encode(ephemeral_public.as_bytes()),
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        wrapped_key: URL_SAFE_NO_PAD.encode(wrapped),
    })
}

pub fn encrypt_bundle(
    key: &[u8; 32],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<BundleCiphertext, CryptoError> {
    let mut nonce = [0_u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = XChaCha20Poly1305::new(key.into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Operation)?;
    Ok(BundleCiphertext {
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
    })
}

pub fn decrypt_bundle(
    key: &[u8; 32],
    encrypted: &BundleCiphertext,
    aad: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    let nonce = decode_24(&encrypted.nonce)?;
    let ciphertext = URL_SAFE_NO_PAD
        .decode(&encrypted.ciphertext)
        .map_err(|_| CryptoError::Encoding)?;
    let plaintext = XChaCha20Poly1305::new(key.into())
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Operation)?;
    Ok(Zeroizing::new(plaintext))
}

fn derive_wrapping_key(shared: &[u8; 32], aad: &[u8]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let hkdf = Hkdf::<Sha256>::new(Some(aad), shared);
    let mut key = Zeroizing::new([0_u8; 32]);
    hkdf.expand(WRAP_INFO, &mut *key)
        .map_err(|_| CryptoError::Operation)?;
    Ok(key)
}

fn decode_32(value: &str) -> Result<[u8; 32], CryptoError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| CryptoError::Encoding)?
        .try_into()
        .map_err(|_| CryptoError::Encoding)
}

fn decode_24(value: &str) -> Result<[u8; 24], CryptoError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| CryptoError::Encoding)?
        .try_into()
        .map_err(|_| CryptoError::Encoding)
}

impl Drop for DeviceEncryptionKey {
    fn drop(&mut self) {
        let mut bytes = self.secret.to_bytes();
        bytes.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_wrap_round_trip() -> Result<(), CryptoError> {
        let device = DeviceEncryptionKey::generate();
        let bundle_key = generate_bundle_key();
        let wrapped = wrap_bundle_key(&bundle_key, &device.public_key(), b"version-1")?;
        let unwrapped = device.unwrap_bundle_key(&wrapped, b"version-1")?;
        assert_eq!(&*bundle_key, &*unwrapped);
        Ok(())
    }

    #[test]
    fn bundle_aad_is_bound() -> Result<(), CryptoError> {
        let key = generate_bundle_key();
        let encrypted = encrypt_bundle(&key, b"private strategy", b"v1")?;
        assert_eq!(
            &*decrypt_bundle(&key, &encrypted, b"v1")?,
            b"private strategy"
        );
        assert!(decrypt_bundle(&key, &encrypted, b"v2").is_err());
        Ok(())
    }
}
