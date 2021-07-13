use std::{
    io,
    ops::{Deref, DerefMut},
    str,
    sync::Arc,
};

use bytes::BytesMut;
use ring::{aead, aead::quic::HeaderProtectionKey, hkdf, hmac};
pub use rustls::TLSError;
use rustls::{
    self,
    quic::{ClientQuicExt, PacketKey, ServerQuicExt},
    Session,
};
use webpki::DNSNameRef;

use crate::{
    crypto::{self, CryptoError, ExportKeyingMaterialError, KeyPair, Keys},
    transport_parameters::TransportParameters,
    CertificateChain, ConnectError, ConnectionId, Side, TransportError, TransportErrorCode,
};

/// A rustls TLS session
#[derive(Debug)]
pub struct TlsSession {
    using_alpn: bool,
    got_handshake_data: bool,
    inner: SessionKind,
}

#[derive(Debug)]
enum SessionKind {
    Client(rustls::ClientSession),
    Server(rustls::ServerSession),
}

impl TlsSession {
    fn side(&self) -> Side {
        match self.inner {
            SessionKind::Client(_) => Side::Client,
            SessionKind::Server(_) => Side::Server,
        }
    }
}

impl crypto::Session for TlsSession {
    type HandshakeData = HandshakeData;
    type Identity = CertificateChain;
    type ClientConfig = Arc<rustls::ClientConfig>;
    type HmacKey = hmac::Key;
    type HandshakeTokenKey = hkdf::Prk;
    type PacketKey = PacketKey;
    type HeaderKey = HeaderProtectionKey;
    type ServerConfig = Arc<rustls::ServerConfig>;

    fn initial_keys(dst_cid: &ConnectionId, side: Side) -> Keys<Self> {
        const INITIAL_SALT: [u8; 20] = [
            0x38, 0x76, 0x2c, 0xf7, 0xf5, 0x59, 0x34, 0xb3, 0x4d, 0x17, 0x9a, 0xe6, 0xa4, 0xc8,
            0x0c, 0xad, 0xcc, 0xbb, 0x7f, 0x0a,
        ];

        let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &INITIAL_SALT);
        let keys = rustls::quic::Keys::initial(&salt, dst_cid, side.is_client());
        Keys {
            header: KeyPair {
                local: keys.local.header,
                remote: keys.remote.header,
            },
            packet: KeyPair {
                local: keys.local.packet,
                remote: keys.remote.packet,
            },
        }
    }

    fn handshake_data(&self) -> Option<HandshakeData> {
        if !self.got_handshake_data {
            return None;
        }
        Some(HandshakeData {
            protocol: self.get_alpn_protocol().map(|x| x.into()),
            server_name: match self.inner {
                SessionKind::Client(_) => None,
                SessionKind::Server(ref session) => session.get_sni_hostname().map(|x| x.into()),
            },
        })
    }

    fn peer_identity(&self) -> Option<CertificateChain> {
        self.get_peer_certificates().map(|v| v.into())
    }

    fn early_crypto(&self) -> Option<(Self::HeaderKey, Self::PacketKey)> {
        let keys = self.get_0rtt_keys()?;
        Some((keys.header, keys.packet))
    }

    fn early_data_accepted(&self) -> Option<bool> {
        match self.inner {
            SessionKind::Client(ref session) => Some(session.is_early_data_accepted()),
            _ => None,
        }
    }

    fn is_handshaking(&self) -> bool {
        match self.inner {
            SessionKind::Client(ref session) => session.is_handshaking(),
            SessionKind::Server(ref session) => session.is_handshaking(),
        }
    }

    fn read_handshake(&mut self, buf: &[u8]) -> Result<bool, TransportError> {
        self.read_hs(buf).map_err(|e| {
            if let Some(alert) = self.get_alert() {
                TransportError {
                    code: TransportErrorCode::crypto(alert.get_u8()),
                    frame: None,
                    reason: e.to_string(),
                }
            } else {
                TransportError::PROTOCOL_VIOLATION(format!("TLS error: {}", e))
            }
        })?;
        if !self.got_handshake_data {
            // Hack around the lack of an explicit signal from rustls to reflect ClientHello being
            // ready on incoming connections, or ALPN negotiation completing on outgoing
            // connections.
            let have_server_name = match self.inner {
                SessionKind::Client(_) => false,
                SessionKind::Server(ref session) => session.get_sni_hostname().is_some(),
            };
            if self.get_alpn_protocol().is_some() || have_server_name || !self.is_handshaking() {
                self.got_handshake_data = true;
                if self.using_alpn && self.get_alpn_protocol().is_none() {
                    // rustls ignores total ALPN failure for compat, but QUIC gets a fresh start
                    return Err(TransportError {
                        code: TransportErrorCode::crypto(0x78),
                        frame: None,
                        reason: "ALPN negotiation failed".into(),
                    });
                }
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        match self.get_quic_transport_parameters() {
            None => Ok(None),
            Some(buf) => match TransportParameters::read(self.side(), &mut io::Cursor::new(buf)) {
                Ok(params) => Ok(Some(params)),
                Err(e) => Err(e.into()),
            },
        }
    }

    fn write_handshake(&mut self, buf: &mut Vec<u8>) -> Option<Keys<Self>> {
        let keys = self.write_hs(buf)?;
        Some(Keys {
            header: KeyPair {
                local: keys.local.header,
                remote: keys.remote.header,
            },
            packet: KeyPair {
                local: keys.local.packet,
                remote: keys.remote.packet,
            },
        })
    }

    fn next_1rtt_keys(&mut self) -> Option<KeyPair<Self::PacketKey>> {
        let keys = (**self).next_1rtt_keys();
        Some(KeyPair {
            local: keys.local,
            remote: keys.remote,
        })
    }

    fn retry_tag(orig_dst_cid: &ConnectionId, packet: &[u8]) -> [u8; 16] {
        let mut pseudo_packet = Vec::with_capacity(packet.len() + orig_dst_cid.len() + 1);
        pseudo_packet.push(orig_dst_cid.len() as u8);
        pseudo_packet.extend_from_slice(orig_dst_cid);
        pseudo_packet.extend_from_slice(packet);

        let nonce = aead::Nonce::assume_unique_for_key(RETRY_INTEGRITY_NONCE);
        let key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::AES_128_GCM, &RETRY_INTEGRITY_KEY).unwrap(),
        );

        let tag = key
            .seal_in_place_separate_tag(nonce, aead::Aad::from(pseudo_packet), &mut [])
            .unwrap();
        let mut result = [0; 16];
        result.copy_from_slice(tag.as_ref());
        result
    }

    fn is_valid_retry(orig_dst_cid: &ConnectionId, header: &[u8], payload: &[u8]) -> bool {
        let tag_start = match payload.len().checked_sub(16) {
            Some(x) => x,
            None => return false,
        };

        let mut pseudo_packet =
            Vec::with_capacity(header.len() + payload.len() + orig_dst_cid.len() + 1);
        pseudo_packet.push(orig_dst_cid.len() as u8);
        pseudo_packet.extend_from_slice(orig_dst_cid);
        pseudo_packet.extend_from_slice(header);
        let tag_start = tag_start + pseudo_packet.len();
        pseudo_packet.extend_from_slice(payload);

        let nonce = aead::Nonce::assume_unique_for_key(RETRY_INTEGRITY_NONCE);
        let key = aead::LessSafeKey::new(
            aead::UnboundKey::new(&aead::AES_128_GCM, &RETRY_INTEGRITY_KEY).unwrap(),
        );

        let (aad, tag) = pseudo_packet.split_at_mut(tag_start);
        key.open_in_place(nonce, aead::Aad::from(aad), tag).is_ok()
    }

    fn export_keying_material(
        &self,
        output: &mut [u8],
        label: &[u8],
        context: &[u8],
    ) -> Result<(), ExportKeyingMaterialError> {
        let session: &dyn rustls::Session = match &self.inner {
            SessionKind::Client(s) => s,
            SessionKind::Server(s) => s,
        };
        session
            .export_keying_material(output, label, Some(context))
            .map_err(|_| ExportKeyingMaterialError)
    }
}

impl Deref for TlsSession {
    type Target = dyn rustls::Session;
    fn deref(&self) -> &Self::Target {
        match self.inner {
            SessionKind::Client(ref session) => session,
            SessionKind::Server(ref session) => session,
        }
    }
}

impl DerefMut for TlsSession {
    fn deref_mut(&mut self) -> &mut (dyn rustls::Session + 'static) {
        match self.inner {
            SessionKind::Client(ref mut session) => session,
            SessionKind::Server(ref mut session) => session,
        }
    }
}

const RETRY_INTEGRITY_KEY: [u8; 16] = [
    0xbe, 0x0c, 0x69, 0x0b, 0x9f, 0x66, 0x57, 0x5a, 0x1d, 0x76, 0x6b, 0x54, 0xe3, 0x68, 0xc8, 0x4e,
];
const RETRY_INTEGRITY_NONCE: [u8; 12] = [
    0x46, 0x15, 0x99, 0xd3, 0x5d, 0x63, 0x2b, 0xf2, 0x23, 0x98, 0x25, 0xbb,
];

/// Authentication data for (rustls) TLS session
pub struct HandshakeData {
    /// The negotiated application protocol, if ALPN is in use
    ///
    /// Guaranteed to be set if a nonempty list of protocols was specified for this connection.
    pub protocol: Option<Vec<u8>>,
    /// The server name specified by the client, if any
    ///
    /// Always `None` for outgoing connections
    pub server_name: Option<String>,
}

impl crypto::ClientConfig<TlsSession> for Arc<rustls::ClientConfig> {
    fn new() -> Self {
        let mut cfg = rustls::ClientConfig::with_ciphersuites(&QUIC_CIPHER_SUITES);
        cfg.versions = vec![rustls::ProtocolVersion::TLSv1_3];
        cfg.enable_early_data = true;
        #[cfg(feature = "native-certs")]
        match rustls_native_certs::load_native_certs() {
            Ok(x) => {
                cfg.root_store = x;
            }
            Err((Some(x), e)) => {
                cfg.root_store = x;
                tracing::warn!("couldn't load some default trust roots: {}", e);
            }
            Err((None, e)) => {
                tracing::warn!("couldn't load any default trust roots: {}", e);
            }
        }
        #[cfg(feature = "certificate-transparency")]
        {
            cfg.ct_logs = Some(&ct_logs::LOGS);
        }
        Arc::new(cfg)
    }

    fn start_session(
        &self,
        server_name: &str,
        params: &TransportParameters,
    ) -> Result<TlsSession, ConnectError> {
        let pki_server_name = DNSNameRef::try_from_ascii_str(server_name)
            .map_err(|_| ConnectError::InvalidDnsName(server_name.into()))?;
        Ok(TlsSession {
            using_alpn: !self.alpn_protocols.is_empty(),
            got_handshake_data: false,
            inner: SessionKind::Client(rustls::ClientSession::new_quic(
                self,
                pki_server_name,
                to_vec(params),
            )),
        })
    }
}

impl crypto::ServerConfig<TlsSession> for Arc<rustls::ServerConfig> {
    fn new() -> Self {
        let mut cfg = rustls::ServerConfig::with_ciphersuites(
            rustls::NoClientAuth::new(),
            &QUIC_CIPHER_SUITES,
        );
        cfg.versions = vec![rustls::ProtocolVersion::TLSv1_3];
        cfg.max_early_data_size = u32::max_value();
        Arc::new(cfg)
    }

    fn start_session(&self, params: &TransportParameters) -> TlsSession {
        TlsSession {
            using_alpn: !self.alpn_protocols.is_empty(),
            got_handshake_data: false,
            inner: SessionKind::Server(rustls::ServerSession::new_quic(self, to_vec(params))),
        }
    }
}

fn to_vec(params: &TransportParameters) -> Vec<u8> {
    let mut bytes = Vec::new();
    params.write(&mut bytes);
    bytes
}

impl crypto::PacketKey for PacketKey {
    fn encrypt(&self, packet: u64, buf: &mut [u8], header_len: usize) {
        let (header, payload) = buf.split_at_mut(header_len);
        let (payload, tag_storage) =
            payload.split_at_mut(payload.len() - self.key.algorithm().tag_len());
        let aad = aead::Aad::from(header);
        let nonce = self.iv.nonce_for(packet);
        let tag = self
            .key
            .seal_in_place_separate_tag(nonce, aad, payload)
            .unwrap();
        tag_storage.copy_from_slice(tag.as_ref());
    }

    fn decrypt(
        &self,
        packet: u64,
        header: &[u8],
        payload: &mut BytesMut,
    ) -> Result<(), CryptoError> {
        if payload.len() < self.key.algorithm().tag_len() {
            return Err(CryptoError);
        }

        let payload_len = payload.len();
        let aad = aead::Aad::from(header);
        let nonce = self.iv.nonce_for(packet);
        self.key.open_in_place(nonce, aad, payload.as_mut())?;
        payload.truncate(payload_len - self.key.algorithm().tag_len());
        Ok(())
    }

    fn tag_len(&self) -> usize {
        self.key.algorithm().tag_len()
    }

    fn confidentiality_limit(&self) -> u64 {
        let cipher = self.key.algorithm();
        if cipher == &aead::AES_128_GCM || cipher == &aead::AES_256_GCM {
            2u64.pow(23)
        } else if cipher == &aead::CHACHA20_POLY1305 {
            u64::MAX
        } else {
            panic!("unknown cipher")
        }
    }

    fn integrity_limit(&self) -> u64 {
        let cipher = self.key.algorithm();
        if cipher == &aead::AES_128_GCM || cipher == &aead::AES_256_GCM {
            2u64.pow(52)
        } else if cipher == &aead::CHACHA20_POLY1305 {
            2u64.pow(36)
        } else {
            panic!("unknown cipher")
        }
    }
}

/// Cipher suites suitable for QUIC
///
/// The list is equivalent to TLS1.3 ciphers.
/// It matches the rustls prefernce list that was introduced with
/// https://github.com/ctz/rustls/commit/7117a805e0104705da50259357d8effa7d599e37.
/// This list prefers AES ciphers, which are hardware accelerated on most platforms.
/// This list can be removed if the rustls dependency is updated to a new version
/// which contains the linked change.
static QUIC_CIPHER_SUITES: [&rustls::SupportedCipherSuite; 3] = [
    &rustls::ciphersuite::TLS13_AES_256_GCM_SHA384,
    &rustls::ciphersuite::TLS13_AES_128_GCM_SHA256,
    &rustls::ciphersuite::TLS13_CHACHA20_POLY1305_SHA256,
];
