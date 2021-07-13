use ring::{aead, hkdf, hmac};

use crate::{
    config::ConfigError,
    crypto::{self, CryptoError},
};

impl crypto::HmacKey for hmac::Key {
    const KEY_LEN: usize = 64;
    type Signature = hmac::Tag;

    fn new(key: &[u8]) -> Result<Self, ConfigError> {
        if key.len() == Self::KEY_LEN {
            Ok(hmac::Key::new(hmac::HMAC_SHA256, key))
        } else {
            Err(ConfigError::OutOfBounds)
        }
    }

    fn sign(&self, data: &[u8]) -> Self::Signature {
        hmac::sign(self, data)
    }

    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<(), CryptoError> {
        Ok(hmac::verify(self, data, signature)?)
    }
}

impl crypto::HandshakeTokenKey for hkdf::Prk {
    type AeadKey = ring::aead::LessSafeKey;

    fn aead_from_hkdf(&self, random_bytes: &[u8]) -> Self::AeadKey {
        let mut key_buffer = [0u8; 32];
        let info = [random_bytes];
        let okm = self.expand(&info, hkdf::HKDF_SHA256).unwrap();

        okm.fill(&mut key_buffer).unwrap();

        let key = aead::UnboundKey::new(&aead::AES_256_GCM, &key_buffer).unwrap();
        Self::AeadKey::new(key)
    }

    fn from_secret(bytes: &[u8]) -> Self {
        hkdf::Salt::new(hkdf::HKDF_SHA256, &[]).extract(bytes)
    }
}

impl crypto::AeadKey for aead::LessSafeKey {
    const KEY_LEN: usize = 32;

    fn seal(&self, data: &mut Vec<u8>, additional_data: &[u8]) -> Result<(), CryptoError> {
        let aad = ring::aead::Aad::from(additional_data);
        let zero_nonce = ring::aead::Nonce::assume_unique_for_key([0u8; 12]);
        Ok(self.seal_in_place_append_tag(zero_nonce, aad, data)?)
    }

    fn open<'a>(
        &self,
        data: &'a mut [u8],
        additional_data: &[u8],
    ) -> Result<&'a mut [u8], CryptoError> {
        let aad = ring::aead::Aad::from(additional_data);
        let zero_nonce = ring::aead::Nonce::assume_unique_for_key([0u8; 12]);
        Ok(self.open_in_place(zero_nonce, aad, data)?)
    }
}

impl From<ring::error::Unspecified> for CryptoError {
    fn from(_: ring::error::Unspecified) -> Self {
        CryptoError
    }
}
