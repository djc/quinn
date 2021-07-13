use std::{convert::TryInto, fmt, num::TryFromIntError, sync::Arc, time::Duration};

use rand::RngCore;
use thiserror::Error;

#[cfg(feature = "rustls")]
use crate::crypto::types::{Certificate, CertificateChain, PrivateKey};
use crate::{
    cid_generator::{ConnectionIdGenerator, RandomConnectionIdGenerator},
    congestion,
    crypto::{self, HandshakeTokenKey as _, HmacKey as _},
    VarInt, VarIntBoundsExceeded, DEFAULT_SUPPORTED_VERSIONS,
};

/// Parameters governing the core QUIC state machine
///
/// Default values should be suitable for most internet applications. Applications protocols which
/// forbid remotely-initiated streams should set `max_concurrent_bidi_streams` and
/// `max_concurrent_uni_streams` to zero.
///
/// In some cases, performance or resource requirements can be improved by tuning these values to
/// suit a particular application and/or network connection. In particular, data window sizes can be
/// tuned for a particular expected round trip time, link capacity, and memory availability. Tuning
/// for higher bandwidths and latencies increases worst-case memory consumption, but does not impair
/// performance at lower bandwidths and latencies. The default configuration is tuned for a 100Mbps
/// link with a 100ms round trip time.
pub struct TransportConfig {
    pub(crate) max_concurrent_bidi_streams: VarInt,
    pub(crate) max_concurrent_uni_streams: VarInt,
    pub(crate) max_idle_timeout: Option<Duration>,
    pub(crate) stream_receive_window: VarInt,
    pub(crate) receive_window: VarInt,
    pub(crate) send_window: u64,

    pub(crate) max_tlps: u32,
    pub(crate) packet_threshold: u32,
    pub(crate) time_threshold: f32,
    pub(crate) initial_rtt: Duration,

    pub(crate) persistent_congestion_threshold: u32,
    pub(crate) keep_alive_interval: Option<Duration>,
    pub(crate) crypto_buffer_size: usize,
    pub(crate) allow_spin: bool,
    pub(crate) datagram_receive_buffer_size: Option<usize>,
    pub(crate) datagram_send_buffer_size: usize,

    pub(crate) congestion_controller_factory: Box<dyn congestion::ControllerFactory + Send + Sync>,
}

impl TransportConfig {
    /// Maximum number of bidirectional streams that may be open concurrently
    ///
    /// Must be nonzero for the peer to open any bidirectional streams.
    ///
    /// Worst-case memory use is directly proportional to `max_concurrent_bidi_streams *
    /// stream_receive_window`, with an upper bound proportional to `receive_window`.
    pub fn max_concurrent_bidi_streams(&mut self, value: u64) -> Result<&mut Self, ConfigError> {
        self.max_concurrent_bidi_streams = value.try_into()?;
        Ok(self)
    }

    /// Variant of `max_concurrent_bidi_streams` affecting unidirectional streams
    pub fn max_concurrent_uni_streams(&mut self, value: u64) -> Result<&mut Self, ConfigError> {
        self.max_concurrent_uni_streams = value.try_into()?;
        Ok(self)
    }

    /// Maximum duration of inactivity to accept before timing out the connection.
    ///
    /// The true idle timeout is the minimum of this and the peer's own max idle timeout. `None`
    /// represents an infinite timeout.
    ///
    /// **WARNING**: If a peer or its network path malfunctions or acts maliciously, an infinite
    /// idle timeout can result in permanently hung futures!
    pub fn max_idle_timeout(&mut self, value: Option<Duration>) -> Result<&mut Self, ConfigError> {
        if value.map_or(false, |x| x.as_millis() > VarInt::MAX.0 as u128) {
            return Err(ConfigError::OutOfBounds);
        }
        self.max_idle_timeout = value;
        Ok(self)
    }

    /// Maximum number of bytes the peer may transmit without acknowledgement on any one stream
    /// before becoming blocked.
    ///
    /// This should be set to at least the expected connection latency multiplied by the maximum
    /// desired throughput. Setting this smaller than `receive_window` helps ensure that a single
    /// stream doesn't monopolize receive buffers, which may otherwise occur if the application
    /// chooses not to read from a large stream for a time while still requiring data on other
    /// streams.
    pub fn stream_receive_window(&mut self, value: u64) -> Result<&mut Self, ConfigError> {
        self.stream_receive_window = value.try_into()?;
        Ok(self)
    }

    /// Maximum number of bytes the peer may transmit across all streams of a connection before
    /// becoming blocked.
    ///
    /// This should be set to at least the expected connection latency multiplied by the maximum
    /// desired throughput. Larger values can be useful to allow maximum throughput within a
    /// stream while another is blocked.
    pub fn receive_window(&mut self, value: u64) -> Result<&mut Self, ConfigError> {
        self.receive_window = value.try_into()?;
        Ok(self)
    }

    /// Maximum number of bytes to transmit to a peer without acknowledgment
    ///
    /// Provides an upper bound on memory when communicating with peers that issue large amounts of
    /// flow control credit. Endpoints that wish to handle large numbers of connections robustly
    /// should take care to set this low enough to guarantee memory exhaustion does not occur if
    /// every connection uses the entire window.
    pub fn send_window(&mut self, value: u64) -> &mut Self {
        self.send_window = value;
        self
    }

    /// Maximum number of tail loss probes before an RTO fires.
    pub fn max_tlps(&mut self, value: u32) -> &mut Self {
        self.max_tlps = value;
        self
    }

    /// Maximum reordering in packet number space before FACK style loss detection considers a
    /// packet lost. Should not be less than 3, per RFC5681.
    pub fn packet_threshold(&mut self, value: u32) -> &mut Self {
        self.packet_threshold = value;
        self
    }

    /// Maximum reordering in time space before time based loss detection considers a packet lost,
    /// as a factor of RTT
    pub fn time_threshold(&mut self, value: f32) -> &mut Self {
        self.time_threshold = value;
        self
    }

    /// The RTT used before an RTT sample is taken
    pub fn initial_rtt(&mut self, value: Duration) -> &mut Self {
        self.initial_rtt = value;
        self
    }

    /// Number of consecutive PTOs after which network is considered to be experiencing persistent congestion.
    pub fn persistent_congestion_threshold(&mut self, value: u32) -> &mut Self {
        self.persistent_congestion_threshold = value;
        self
    }

    /// Period of inactivity before sending a keep-alive packet
    ///
    /// Keep-alive packets prevent an inactive but otherwise healthy connection from timing out.
    ///
    /// `None` to disable, which is the default. Only one side of any given connection needs keep-alive
    /// enabled for the connection to be preserved. Must be set lower than the idle_timeout of both
    /// peers to be effective.
    pub fn keep_alive_interval(&mut self, value: Option<Duration>) -> &mut Self {
        self.keep_alive_interval = value;
        self
    }

    /// Maximum quantity of out-of-order crypto layer data to buffer
    pub fn crypto_buffer_size(&mut self, value: usize) -> &mut Self {
        self.crypto_buffer_size = value;
        self
    }

    /// Whether the implementation is permitted to set the spin bit on this connection
    ///
    /// This allows passive observers to easily judge the round trip time of a connection, which can
    /// be useful for network administration but sacrifices a small amount of privacy.
    pub fn allow_spin(&mut self, value: bool) -> &mut Self {
        self.allow_spin = value;
        self
    }

    /// Maximum number of incoming application datagram bytes to buffer, or None to disable
    /// incoming datagrams
    ///
    /// The peer is forbidden to send single datagrams larger than this size. If the aggregate size
    /// of all datagrams that have been received from the peer but not consumed by the application
    /// exceeds this value, old datagrams are dropped until it is no longer exceeded.
    pub fn datagram_receive_buffer_size(&mut self, value: Option<usize>) -> &mut Self {
        self.datagram_receive_buffer_size = value;
        self
    }

    /// Maximum number of outgoing application datagram bytes to buffer
    ///
    /// While datagrams are sent ASAP, it is possible for an application to generate data faster
    /// than the link, or even the underlying hardware, can transmit them. This limits the amount of
    /// memory that may be consumed in that case. When the send buffer is full and a new datagram is
    /// sent, older datagrams are dropped until sufficient space is available.
    pub fn datagram_send_buffer_size(&mut self, value: usize) -> &mut Self {
        self.datagram_send_buffer_size = value;
        self
    }

    /// How to construct new `congestion::Controller`s
    ///
    /// Typically the refcounted configuration of a `congestion::Controller`,
    /// e.g. a `congestion::NewRenoConfig`.
    ///
    /// # Example
    /// ```
    /// # use quinn_proto::*; use std::sync::Arc;
    /// let mut config = TransportConfig::default();
    /// config.congestion_controller_factory(Arc::new(congestion::NewRenoConfig::default()));
    /// ```
    pub fn congestion_controller_factory(
        &mut self,
        factory: impl congestion::ControllerFactory + Send + Sync + 'static,
    ) -> &mut Self {
        self.congestion_controller_factory = Box::new(factory);
        self
    }
}

impl Default for TransportConfig {
    fn default() -> Self {
        const EXPECTED_RTT: u32 = 100; // ms
        const MAX_STREAM_BANDWIDTH: u32 = 12500 * 1000; // bytes/s
                                                        // Window size needed to avoid pipeline
                                                        // stalls
        const STREAM_RWND: u32 = MAX_STREAM_BANDWIDTH / 1000 * EXPECTED_RTT;

        TransportConfig {
            max_concurrent_bidi_streams: 100u32.into(),
            max_concurrent_uni_streams: 100u32.into(),
            max_idle_timeout: Some(Duration::from_millis(10_000)),
            stream_receive_window: STREAM_RWND.into(),
            receive_window: VarInt::MAX,
            send_window: (8 * STREAM_RWND).into(),

            max_tlps: 2,
            packet_threshold: 3,
            time_threshold: 9.0 / 8.0,
            initial_rtt: Duration::from_millis(333), // per spec, intentionally distinct from EXPECTED_RTT

            persistent_congestion_threshold: 3,
            keep_alive_interval: None,
            crypto_buffer_size: 16 * 1024,
            allow_spin: true,
            datagram_receive_buffer_size: Some(STREAM_RWND as usize),
            datagram_send_buffer_size: 1024 * 1024,

            congestion_controller_factory: Box::new(Arc::new(congestion::NewRenoConfig::default())),
        }
    }
}

impl fmt::Debug for TransportConfig {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("TranportConfig")
            .field(
                "max_concurrent_bidi_streams",
                &self.max_concurrent_bidi_streams,
            )
            .field(
                "max_concurrent_uni_streams",
                &self.max_concurrent_uni_streams,
            )
            .field("max_idle_timeout", &self.max_idle_timeout)
            .field("stream_receive_window", &self.stream_receive_window)
            .field("receive_window", &self.receive_window)
            .field("send_window", &self.send_window)
            .field("max_tlps", &self.max_tlps)
            .field("packet_threshold", &self.packet_threshold)
            .field("time_threshold", &self.time_threshold)
            .field("initial_rtt", &self.initial_rtt)
            .field(
                "persistent_congestion_threshold",
                &self.persistent_congestion_threshold,
            )
            .field("keep_alive_interval", &self.keep_alive_interval)
            .field("crypto_buffer_size", &self.crypto_buffer_size)
            .field("allow_spin", &self.allow_spin)
            .field(
                "datagram_receive_buffer_size",
                &self.datagram_receive_buffer_size,
            )
            .field("datagram_send_buffer_size", &self.datagram_send_buffer_size)
            .field("congestion_controller_factory", &"[ opaque ]")
            .finish()
    }
}

/// Global configuration for the endpoint, affecting all connections
///
/// Default values should be suitable for most internet applications.
pub struct EndpointConfig<S>
where
    S: crypto::Session,
{
    pub(crate) reset_key: Arc<S::HmacKey>,
    pub(crate) max_udp_payload_size: VarInt,
    /// CID generator factory
    ///
    /// Create a cid generator for local cid in Endpoint struct
    pub(crate) connection_id_generator_factory:
        Arc<dyn Fn() -> Box<dyn ConnectionIdGenerator> + Send + Sync>,
    pub(crate) supported_versions: Vec<u32>,
    pub(crate) initial_version: u32,
}

impl<S> EndpointConfig<S>
where
    S: crypto::Session,
{
    /// Create a default config with a particular `reset_key`
    pub fn new(reset_key: S::HmacKey) -> Self {
        let cid_factory: fn() -> Box<dyn ConnectionIdGenerator> =
            || Box::new(RandomConnectionIdGenerator::default());
        Self {
            reset_key: Arc::new(reset_key),
            max_udp_payload_size: 1480u32.into(), // Typical internet MTU minus IPv4 and UDP overhead, rounded up to a multiple of 8
            connection_id_generator_factory: Arc::new(cid_factory),
            initial_version: DEFAULT_SUPPORTED_VERSIONS[0],
            supported_versions: DEFAULT_SUPPORTED_VERSIONS.to_vec(),
        }
    }

    /// Supply a custom connection ID generator factory
    ///
    /// Called once by each `Endpoint` constructed from this configuration to obtain the CID
    /// generator which will be used to generate the CIDs used for incoming packets on all
    /// connections involving that  `Endpoint`. A custom CID generator allows applications to embed
    /// information in local connection IDs, e.g. to support stateless packet-level load balancers.
    ///
    /// `EndpointConfig::new()` applies a default random CID generator factory. This functions
    /// accepts any customized CID generator to reset CID generator factory that implements
    /// the `ConnectionIdGenerator` trait.
    pub fn cid_generator<F: Fn() -> Box<dyn ConnectionIdGenerator> + Send + Sync + 'static>(
        &mut self,
        factory: F,
    ) -> &mut Self {
        self.connection_id_generator_factory = Arc::new(factory);
        self
    }

    /// Private key used to send authenticated connection resets to peers who were
    /// communicating with a previous instance of this endpoint.
    pub fn reset_key(&mut self, value: &[u8]) -> Result<&mut Self, ConfigError> {
        self.reset_key = Arc::new(S::HmacKey::new(value)?);
        Ok(self)
    }

    /// Maximum UDP payload size accepted from peers. Excludes UDP and IP overhead.
    ///
    /// The default is suitable for typical internet applications. Applications which expect to run
    /// on networks supporting Ethernet jumbo frames or similar should set this appropriately.
    pub fn max_udp_payload_size(&mut self, value: u64) -> Result<&mut Self, ConfigError> {
        self.max_udp_payload_size = value.try_into()?;
        Ok(self)
    }

    /// Get the current value of `max_udp_payload_size`
    ///
    /// While most parameters don't need to be readable, this must be exposed to allow higher-level
    /// layers, e.g. the `quinn` crate, to determine how large a receive buffer to allocate to
    /// support an externally-defined `EndpointConfig`.
    ///
    /// While `get_` accessors are typically unidiomatic in Rust, we favor concision for setters,
    /// which will be used far more heavily.
    #[doc(hidden)]
    pub fn get_max_udp_payload_size(&self) -> u64 {
        self.max_udp_payload_size.into()
    }

    /// Override supported QUIC versions
    pub fn supported_versions(
        &mut self,
        supported_versions: Vec<u32>,
        initial_version: u32,
    ) -> Result<&mut Self, ConfigError> {
        if !supported_versions.contains(&initial_version) {
            return Err(ConfigError::OutOfBounds);
        }
        self.supported_versions = supported_versions;
        self.initial_version = initial_version;
        Ok(self)
    }
}

impl<S: crypto::Session> fmt::Debug for EndpointConfig<S> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("EndpointConfig")
            .field("reset_key", &"[ elided ]")
            .field("max_udp_payload_size", &self.max_udp_payload_size)
            .field("cid_generator_factory", &"[ elided ]")
            .field("supported_versions", &self.supported_versions)
            .field("initial_version", &self.initial_version)
            .finish()
    }
}

impl<S: crypto::Session> Default for EndpointConfig<S> {
    fn default() -> Self {
        let mut reset_key = vec![0; S::HmacKey::KEY_LEN];
        rand::thread_rng().fill_bytes(&mut reset_key);
        Self::new(
            S::HmacKey::new(&reset_key)
                .expect("HMAC key rejected random bytes; use EndpointConfig::new instead"),
        )
    }
}

impl<S: crypto::Session> Clone for EndpointConfig<S> {
    fn clone(&self) -> Self {
        Self {
            reset_key: self.reset_key.clone(),
            max_udp_payload_size: self.max_udp_payload_size,
            connection_id_generator_factory: self.connection_id_generator_factory.clone(),
            supported_versions: self.supported_versions.clone(),
            initial_version: self.initial_version,
        }
    }
}

/// Parameters governing incoming connections
///
/// Default values should be suitable for most internet applications.
pub struct ServerConfig<S>
where
    S: crypto::Session,
{
    /// Transport configuration to use for incoming connections
    pub transport: Arc<TransportConfig>,

    /// TLS configuration used for incoming connections.
    ///
    /// Must be set to use TLS 1.3 only.
    pub crypto: S::ServerConfig,

    /// Used to generate one-time AEAD keys to protect handshake tokens
    pub(crate) token_key: Arc<S::HandshakeTokenKey>,

    /// Whether to require clients to prove ownership of an address before committing resources.
    ///
    /// Introduces an additional round-trip to the handshake to make denial of service attacks more difficult.
    pub(crate) use_stateless_retry: bool,
    /// Microseconds after a stateless retry token was issued for which it's considered valid.
    pub(crate) retry_token_lifetime: u64,

    /// Maximum number of concurrent connections
    pub(crate) concurrent_connections: u32,

    /// Whether to allow clients to migrate to new addresses
    ///
    /// Improves behavior for clients that move between different internet connections or suffer NAT
    /// rebinding. Enabled by default.
    pub(crate) migration: bool,
}

impl<S> ServerConfig<S>
where
    S: crypto::Session,
{
    /// Create a default config with a particular `master_key`
    pub fn new(crypto: S::ServerConfig, prk: S::HandshakeTokenKey) -> Self {
        Self {
            transport: Arc::new(TransportConfig::default()),
            crypto,

            token_key: Arc::new(prk),
            use_stateless_retry: false,
            retry_token_lifetime: 15_000_000,

            concurrent_connections: 100_000,

            migration: true,
        }
    }

    /// Private key used to authenticate data included in handshake tokens.
    pub fn token_key(&mut self, master_key: &[u8]) -> Result<&mut Self, ConfigError> {
        self.token_key = Arc::new(S::HandshakeTokenKey::from_secret(&master_key));
        Ok(self)
    }

    /// Whether to require clients to prove ownership of an address before committing resources.
    ///
    /// Introduces an additional round-trip to the handshake to make denial of service attacks more difficult.
    pub fn use_stateless_retry(&mut self, value: bool) -> &mut Self {
        self.use_stateless_retry = value;
        self
    }

    /// Microseconds after a stateless retry token was issued for which it's considered valid.
    pub fn retry_token_lifetime(&mut self, value: u64) -> &mut Self {
        self.retry_token_lifetime = value;
        self
    }

    /// Maximum number of simultaneous connections to accept.
    ///
    /// New incoming connections are only accepted if the total number of incoming or outgoing
    /// connections is less than this. Outgoing connections are unaffected.
    pub fn concurrent_connections(&mut self, value: u32) -> &mut Self {
        self.concurrent_connections = value;
        self
    }

    /// Whether to allow clients to migrate to new addresses
    ///
    /// Improves behavior for clients that move between different internet connections or suffer NAT
    /// rebinding. Enabled by default.
    pub fn migration(&mut self, value: bool) -> &mut Self {
        self.migration = value;
        self
    }
}

#[cfg(feature = "rustls")]
impl ServerConfig<crypto::rustls::TlsSession> {
    /// Create a server config with the given certificate chain to be presented to clients
    ///
    /// Uses a randomized handshake token key.
    pub fn with_single_cert(
        cert_chain: CertificateChain,
        key: PrivateKey,
    ) -> Result<Self, rustls::Error> {
        let mut crypto = rustls::ServerConfig::builder()
            .with_cipher_suites(&crypto::rustls::QUIC_CIPHER_SUITES)
            .with_safe_default_kx_groups()
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(cert_chain.certs, key.inner)?;
        crypto.max_early_data_size = u32::max_value();

        Ok(Self::with_crypto(Arc::new(crypto)))
    }

    /// Create a server config with the given [`rustls::ServerConfig`]
    ///
    /// Uses a randomized handshake token key.
    pub fn with_crypto(crypto: Arc<rustls::ServerConfig>) -> Self {
        let rng = &mut rand::thread_rng();
        let mut master_key = [0u8; 64];
        rng.fill_bytes(&mut master_key);
        let master_key =
            <crypto::rustls::TlsSession as crypto::Session>::HandshakeTokenKey::from_secret(
                &master_key,
            );

        Self::new(crypto, master_key)
    }
}

impl<S> fmt::Debug for ServerConfig<S>
where
    S: crypto::Session,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("ServerConfig<T>")
            .field("transport", &self.transport)
            .field("crypto", &"ServerConfig { elided }")
            .field("token_key", &"[ elided ]")
            .field("use_stateless_retry", &self.use_stateless_retry)
            .field("retry_token_lifetime", &self.retry_token_lifetime)
            .field("concurrent_connections", &self.concurrent_connections)
            .field("migration", &self.migration)
            .finish()
    }
}

impl<S> Clone for ServerConfig<S>
where
    S: crypto::Session,
    S::ServerConfig: Clone,
{
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            crypto: self.crypto.clone(),
            token_key: self.token_key.clone(),
            use_stateless_retry: self.use_stateless_retry,
            retry_token_lifetime: self.retry_token_lifetime,
            concurrent_connections: self.concurrent_connections,
            migration: self.migration,
        }
    }
}

/// Configuration for outgoing connections
///
/// Default values should be suitable for most internet applications.
pub struct ClientConfig<S>
where
    S: crypto::Session,
{
    /// Transport configuration to use
    pub transport: Arc<TransportConfig>,

    /// Cryptographic configuration to use
    pub crypto: S::ClientConfig,
}

#[cfg(feature = "rustls")]
impl ClientConfig<crypto::rustls::TlsSession> {
    /// Create a client configuration that trusts the platform's native roots
    #[cfg(feature = "native-certs")]
    pub fn with_native_roots() -> Self {
        let mut roots = rustls::RootCertStore::empty();
        match rustls_native_certs::load_native_certs() {
            Ok(certs) => {
                for cert in certs {
                    if let Err(e) = roots.add(&rustls::Certificate(cert.0)) {
                        tracing::warn!("failed to parse trust anchor: {}", e);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("couldn't load any default trust roots: {}", e);
            }
        };

        Self::new(roots, None)
    }

    /// Create a client configuration that trusts specified trust anchors
    ///
    /// If `ct_logs` is `None`, will use the default Certificate Transparency logs depending on
    /// whether the `ct-logs` features is enabled or not. Otherwise, will use the specified logs.
    pub fn with_root_certificates(
        certs: impl IntoIterator<Item = Certificate>,
        ct_logs: Option<&'static [&'static sct::Log]>,
    ) -> Result<Self, webpki::Error> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in certs {
            roots.add(&cert.inner)?;
        }

        Ok(Self::new(roots, ct_logs))
    }

    fn new(
        roots: rustls::RootCertStore,
        #[allow(unused_mut)] mut ct_logs: Option<&'static [&'static sct::Log]>,
    ) -> Self {
        #[cfg(feature = "certificate-transparency")]
        {
            if ct_logs.is_none() {
                ct_logs = Some(&ct_logs::LOGS);
            }
        }

        let mut cfg = rustls::ClientConfig::builder()
            .with_cipher_suites(&crypto::rustls::QUIC_CIPHER_SUITES)
            .with_safe_default_kx_groups()
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots, ct_logs.unwrap_or(&[]))
            .with_no_client_auth();
        cfg.enable_early_data = true;

        Self {
            transport: Arc::new(TransportConfig::default()),
            crypto: Arc::new(cfg),
        }
    }
}

impl<S> Clone for ClientConfig<S>
where
    S: crypto::Session,
    S::ClientConfig: Clone,
{
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            crypto: self.crypto.clone(),
        }
    }
}

impl<S> fmt::Debug for ClientConfig<S>
where
    S: crypto::Session,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("ClientConfig<T>")
            .field("transport", &self.transport)
            .field("crypto", &"ClientConfig { elided }")
            .finish()
    }
}

/// Errors in the configuration of an endpoint
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// Value exceeds supported bounds
    #[error("value exceeds supported bounds")]
    OutOfBounds,
}

impl From<TryFromIntError> for ConfigError {
    fn from(_: TryFromIntError) -> Self {
        ConfigError::OutOfBounds
    }
}

impl From<VarIntBoundsExceeded> for ConfigError {
    fn from(_: VarIntBoundsExceeded) -> Self {
        ConfigError::OutOfBounds
    }
}
