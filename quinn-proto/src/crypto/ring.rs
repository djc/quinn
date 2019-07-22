use std::io;

use bytes::{Buf, BufMut, BytesMut};
use ring::aead::quic::{HeaderProtectionKey, AES_128, AES_256, CHACHA20};
use ring::aead::{self, Aad, Nonce};
use ring::digest;
use ring::hkdf;
use ring::hmac::{self, SigningKey};

use crate::packet::{PacketNumber, LONG_HEADER_FORM};
use crate::shared::{ConfigError, ConnectionId};
use crate::{crypto, Side};

/// Keys for encrypting and decrypting packet payloads
pub struct Crypto {
    pub(crate) local_secret: Vec<u8>,
    local_iv: Vec<u8>,
    sealing_key: aead::SealingKey,
    pub(crate) remote_secret: Vec<u8>,
    remote_iv: Vec<u8>,
    opening_key: aead::OpeningKey,
    digest: &'static digest::Algorithm,
}

impl Crypto {
    pub(crate) fn new(
        side: Side,
        digest: &'static digest::Algorithm,
        cipher: &'static aead::Algorithm,
        client_secret: Vec<u8>,
        server_secret: Vec<u8>,
    ) -> Self {
        let (local_secret, remote_secret) = match side {
            Side::Client => (client_secret, server_secret),
            Side::Server => (server_secret, client_secret),
        };

        let (local_key, local_iv) = Self::get_keys(digest, cipher, &local_secret);
        let (remote_key, remote_iv) = Self::get_keys(digest, cipher, &remote_secret);
        Crypto {
            local_secret,
            sealing_key: aead::SealingKey::new(cipher, &local_key).unwrap(),
            local_iv,
            remote_secret,
            opening_key: aead::OpeningKey::new(cipher, &remote_key).unwrap(),
            remote_iv,
            digest,
        }
    }

    fn write_nonce(&self, iv: &[u8], number: u64, out: &mut [u8]) {
        let out = {
            let mut write = io::Cursor::new(out);
            write.put_u32_be(0);
            write.put_u64_be(number);
            debug_assert_eq!(write.remaining(), 0);
            write.into_inner()
        };
        debug_assert_eq!(out.len(), iv.len());
        for (out, inp) in out.iter_mut().zip(iv.iter()) {
            *out ^= inp;
        }
    }

    fn get_keys(
        digest: &'static digest::Algorithm,
        cipher: &'static aead::Algorithm,
        secret: &[u8],
    ) -> (Vec<u8>, Vec<u8>) {
        let secret_key = SigningKey::new(digest, &secret);

        let mut key = vec![0; cipher.key_len()];
        hkdf_expand(&secret_key, b"quic key", &mut key);

        let mut iv = vec![0; cipher.nonce_len()];
        hkdf_expand(&secret_key, b"quic iv", &mut iv);

        (key, iv)
    }
}

impl crypto::Keys for Crypto {
    type HeaderKeys = RingHeaderCrypto;

    fn new_initial(id: &ConnectionId, side: Side) -> Self {
        let (digest, cipher) = (&digest::SHA256, &aead::AES_128_GCM);
        const CLIENT_LABEL: &[u8] = b"client in";
        const SERVER_LABEL: &[u8] = b"server in";
        let hs_secret = initial_secret(id);

        let client_secret = expanded_initial_secret(&hs_secret, CLIENT_LABEL);
        let server_secret = expanded_initial_secret(&hs_secret, SERVER_LABEL);
        Self::new(side, digest, cipher, client_secret, server_secret)
    }

    fn encrypt(&self, packet: u64, buf: &mut [u8], header_len: usize) {
        let (cipher, iv, key) = (
            self.sealing_key.algorithm(),
            &self.local_iv,
            &self.sealing_key,
        );

        let mut nonce_buf = [0u8; aead::MAX_TAG_LEN];
        let nonce = &mut nonce_buf[..cipher.nonce_len()];
        self.write_nonce(&iv, packet, nonce);

        let (header, payload) = buf.split_at_mut(header_len);
        let header = Aad::from(header);
        let nonce = Nonce::try_assume_unique_for_key(nonce).unwrap();
        aead::seal_in_place(&key, nonce, header, payload, cipher.tag_len()).unwrap();
    }

    fn decrypt(&self, packet: u64, header: &[u8], payload: &mut BytesMut) -> Result<(), ()> {
        if payload.len() < self.tag_len() {
            return Err(());
        }

        let (cipher, iv, key) = (
            self.opening_key.algorithm(),
            &self.remote_iv,
            &self.opening_key,
        );

        let mut nonce_buf = [0u8; aead::MAX_TAG_LEN];
        let nonce = &mut nonce_buf[..cipher.nonce_len()];
        self.write_nonce(&iv, packet, nonce);
        let payload_len = payload.len();

        let header = Aad::from(header);
        let nonce = Nonce::try_assume_unique_for_key(nonce).unwrap();
        aead::open_in_place(&key, nonce, header, 0, payload.as_mut()).map_err(|_| ())?;
        payload.split_off(payload_len - cipher.tag_len());
        Ok(())
    }

    fn header_keys(&self) -> RingHeaderCrypto {
        let local = SigningKey::new(self.digest, &self.local_secret);
        let remote = SigningKey::new(self.digest, &self.remote_secret);
        let cipher = self.sealing_key.algorithm();
        RingHeaderCrypto {
            local: header_key_from_secret(cipher, &local),
            remote: header_key_from_secret(cipher, &remote),
        }
    }

    fn tag_len(&self) -> usize {
        self.sealing_key.algorithm().tag_len()
    }
}

/// Keys for encrypting and decrypting packet headers
pub struct RingHeaderCrypto {
    local: HeaderProtectionKey,
    remote: HeaderProtectionKey,
}

impl crypto::HeaderKeys for RingHeaderCrypto {
    fn decrypt(&self, pn_offset: usize, packet: &mut [u8]) {
        let (header, sample) = packet.split_at_mut(pn_offset + 4);
        let mask = self
            .remote
            .new_mask(&sample[0..self.sample_size()])
            .unwrap();
        if header[0] & LONG_HEADER_FORM == LONG_HEADER_FORM {
            // Long header: 4 bits masked
            header[0] ^= mask[0] & 0x0f;
        } else {
            // Short header: 5 bits masked
            header[0] ^= mask[0] & 0x1f;
        }
        let pn_length = PacketNumber::decode_len(header[0]);
        for (out, inp) in header[pn_offset..pn_offset + pn_length]
            .iter_mut()
            .zip(&mask[1..])
        {
            *out ^= inp;
        }
    }

    fn encrypt(&self, pn_offset: usize, packet: &mut [u8]) {
        let (header, sample) = packet.split_at_mut(pn_offset + 4);
        let mask = self.local.new_mask(&sample[0..self.sample_size()]).unwrap();
        let pn_length = PacketNumber::decode_len(header[0]);
        if header[0] & 0x80 == 0x80 {
            // Long header: 4 bits masked
            header[0] ^= mask[0] & 0x0f;
        } else {
            // Short header: 5 bits masked
            header[0] ^= mask[0] & 0x1f;
        }
        for (out, inp) in header[pn_offset..pn_offset + pn_length]
            .iter_mut()
            .zip(&mask[1..])
        {
            *out ^= inp;
        }
    }

    fn sample_size(&self) -> usize {
        self.local.algorithm().sample_len()
    }
}

fn header_key_from_secret(aead: &aead::Algorithm, secret_key: &SigningKey) -> HeaderProtectionKey {
    const LABEL: &[u8] = b"quic hp";
    if aead == &aead::AES_128_GCM {
        let mut pn = [0; 16];
        hkdf_expand(&secret_key, LABEL, &mut pn);
        HeaderProtectionKey::new(&AES_128, &pn).unwrap()
    } else if aead == &aead::AES_256_GCM {
        let mut pn = [0; 32];
        hkdf_expand(&secret_key, LABEL, &mut pn);
        HeaderProtectionKey::new(&AES_256, &pn).unwrap()
    } else if aead == &aead::CHACHA20_POLY1305 {
        let mut pn = [0; 32];
        hkdf_expand(&secret_key, LABEL, &mut pn);
        HeaderProtectionKey::new(&CHACHA20, &pn).unwrap()
    } else {
        unimplemented!()
    }
}

fn expanded_initial_secret(prk: &SigningKey, label: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; digest::SHA256.output_len];
    hkdf_expand(prk, label, &mut out);
    out
}

fn hkdf_expand(key: &SigningKey, label: &[u8], out: &mut [u8]) {
    let mut info = Vec::with_capacity(2 + 1 + 5 + out.len());
    info.put_u16_be(out.len() as u16);
    const BASE_LABEL: &[u8] = b"tls13 ";
    info.put_u8((BASE_LABEL.len() + label.len()) as u8);
    info.extend_from_slice(BASE_LABEL);
    info.extend_from_slice(&label);
    info.put_u8(0);
    hkdf::expand(key, &info, out);
}

fn initial_secret(conn_id: &ConnectionId) -> SigningKey {
    let key = SigningKey::new(&digest::SHA256, &INITIAL_SALT);
    hkdf::extract(&key, conn_id)
}

const INITIAL_SALT: [u8; 20] = [
    0x7f, 0xbc, 0xdb, 0x0e, 0x7c, 0x66, 0xbb, 0xe9, 0x19, 0x3a, 0x96, 0xcd, 0x21, 0x51, 0x9e, 0xbd,
    0x7a, 0x02, 0x64, 0x4a,
];

impl crypto::HmacKey for hmac::SigningKey {
    const KEY_LEN: usize = 64;
    type Signature = hmac::Signature;

    fn new(key: &[u8]) -> Result<Self, ConfigError> {
        if key.len() == Self::KEY_LEN {
            Ok(hmac::SigningKey::new(&digest::SHA512_256, key))
        } else {
            Err(ConfigError::IllegalValue("key length must be 64 bytes"))
        }
    }

    fn sign(&self, data: &[u8]) -> Self::Signature {
        hmac::sign(self, data)
    }

    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<(), ()> {
        hmac::verify_with_own_key(self, data, signature).map_err(|_| ())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::crypto::{HeaderKeys, Keys};
    use crate::MAX_CID_SIZE;
    use hex_literal::hex;
    use rand;

    #[test]
    fn handshake_crypto_roundtrip() {
        let conn = ConnectionId::random(&mut rand::thread_rng(), MAX_CID_SIZE);
        let client = Crypto::new_initial(&conn, Side::Client);
        let server = Crypto::new_initial(&conn, Side::Server);

        let mut buf = b"headerpayload".to_vec();
        buf.resize(buf.len() + client.tag_len(), 0);
        client.encrypt(0, &mut buf, 6);

        let mut header = BytesMut::from(buf);
        let mut payload = header.split_off(6);
        server.decrypt(0, &header, &mut payload).unwrap();
        assert_eq!(&*payload, b"payload");
    }

    #[test]
    fn key_derivation() {
        let id = ConnectionId::new(&hex!("8394c8f03e515708"));
        let digest = &digest::SHA256;
        let cipher = &aead::AES_128_GCM;
        let initial_secret = initial_secret(&id);
        let client_secret = expanded_initial_secret(&initial_secret, b"client in");
        println!();
        assert_eq!(
            &client_secret[..],
            hex!("7712ead935b044cb18e993a6f7a8c711 19d2439ffdd3b6151ad7f9d9e77e2fb9")
        );
        let (client_key, client_iv) = Crypto::get_keys(digest, cipher, &client_secret);
        assert_eq!(&client_key[..], hex!("07863d9c083786bc86766ce0d02bf93f"));
        assert_eq!(&client_iv[..], hex!("fb33da41a8f297d482df670e"));

        let server_secret = expanded_initial_secret(&initial_secret, b"server in");
        assert_eq!(
            &server_secret[..],
            hex!("dc33b018d3bf848d1a35d9339e2a7049 4e88e82504deb1a1bac5585d48214956")
        );
        let (server_key, server_iv) = Crypto::get_keys(digest, cipher, &server_secret);
        assert_eq!(&server_key[..], hex!("baefa0f549e8f5aee4b9e574dfebf52d"));
        assert_eq!(&server_iv[..], hex!("74fc8e534408a0b3928a3906"));
    }

    #[test]
    fn packet_protection() {
        let id = ConnectionId::new(&hex!("8394c8f03e515708"));
        let server = Crypto::new_initial(&id, Side::Server);
        let server_header = server.header_keys();
        let client = Crypto::new_initial(&id, Side::Client);
        let client_header = client.header_keys();
        let plaintext = hex!(
            "c1ff00001205f067a5502a4262b50040740000
             0d0000000018410a020000560303eefc e7f7b37ba1d1632e96677825ddf73988
             cfc79825df566dc5430b9a045a120013 0100002e00330024001d00209d3c940d
             89690b84d08a60993c144eca684d1081 287c834d5311bcf32bb9da1a002b0002
             0304"
        );
        const HEADER_LEN: usize = 19;
        let protected = hex!(
            "cfff00001205f067a5502a4262b5004074b517
             cacd4600a63aee6eff75daa86546b48f 0c560e2730cb549780493f537a3a6e3b
             de8cbdd2dc6037a784e54651b6b78c76 a65be06d0adc945c134f327c00c28edf
             9dcec89806ebf5087a462b9a856e0ad6 085f14ebc6ffbb7288e691da3fb71b75
             7efcf8a4c83ca4a3f69ecb7a510f2e03 4fcd"
        );
        let mut packet = plaintext.to_vec();
        packet.resize(packet.len() + server.tag_len(), 0);
        server.encrypt(0, &mut packet, HEADER_LEN);
        server_header.encrypt(17, &mut packet);
        assert_eq!(&packet[..], &protected[..]);
        client_header.decrypt(17, &mut packet);
        let (header, payload) = packet.split_at(HEADER_LEN);
        assert_eq!(header, &plaintext[0..HEADER_LEN]);
        let mut payload = BytesMut::from(payload);
        client.decrypt(0, &header, &mut payload).unwrap();
        assert_eq!(&payload, &plaintext[HEADER_LEN..]);
    }

    #[test]
    fn key_derivation_1rtt() {
        // Pre-update test vectors generated by ngtcp2
        let digest = &digest::SHA256;
        let cipher = &aead::AES_128_GCM;
        let onertt = Crypto::new(
            Side::Client,
            digest,
            cipher,
            vec![
                0xb8, 0x76, 0x77, 0x08, 0xf8, 0x77, 0x23, 0x58, 0xa6, 0xea, 0x9f, 0xc4, 0x3e, 0x4a,
                0xdd, 0x2c, 0x96, 0x1b, 0x3f, 0x52, 0x87, 0xa6, 0xd1, 0x46, 0x7e, 0xe0, 0xae, 0xab,
                0x33, 0x72, 0x4d, 0xbf,
            ],
            vec![
                0x42, 0xdc, 0x97, 0x21, 0x40, 0xe0, 0xf2, 0xe3, 0x98, 0x45, 0xb7, 0x67, 0x61, 0x34,
                0x39, 0xdc, 0x67, 0x58, 0xca, 0x43, 0x25, 0x9b, 0x87, 0x85, 0x06, 0x82, 0x4e, 0xb1,
                0xe4, 0x38, 0xd8, 0x55,
            ],
        );

        assert_eq!(
            &onertt.local_iv[..],
            [0xd5, 0x1b, 0x16, 0x6a, 0x3e, 0xc4, 0x6f, 0x7e, 0x5f, 0x93, 0x27, 0x15]
        );
        assert_eq!(
            &onertt.remote_iv[..],
            [0x03, 0xda, 0x92, 0xa0, 0x91, 0x95, 0xe4, 0xbf, 0x87, 0x98, 0xd3, 0x78]
        );
    }
}
