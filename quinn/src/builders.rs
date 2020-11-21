use std::{io, net::SocketAddr, sync::Arc};

use proto::generic::{ClientConfig, EndpointConfig, ServerConfig};
use std::os::windows::io::FromRawSocket;
use thiserror::Error;
use tracing::error;
use winapi::{
    shared::{ws2def, ws2ipdef},
    um::winsock2,
};

use crate::endpoint::{
    into_c_addr, wsa_last_error, Endpoint, EndpointDriver, EndpointRef, Incoming,
};
#[cfg(feature = "rustls")]
use crate::{Certificate, CertificateChain, PrivateKey};

/// Creates a windows socket
pub fn create_wsa_socket(addr: &SocketAddr) -> Result<std::net::UdpSocket, std::io::Error> {
    let raw_socket = unsafe {
        winsock2::WSASocketA(
            if addr.is_ipv4() {
                ws2def::AF_INET
            } else {
                ws2def::AF_INET6
            },
            ws2def::SOCK_DGRAM,
            ws2def::IPPROTO_UDP as i32,
            std::ptr::null_mut(),
            0,
            winsock2::WSA_FLAG_REGISTERED_IO,
        )
    };
    if raw_socket == winsock2::INVALID_SOCKET {
        println!("Invalid socket");
        return Err(wsa_last_error());
    }

    let sock_addr = into_c_addr(*addr);

    if winsock2::SOCKET_ERROR
        == unsafe {
            winsock2::bind(
                raw_socket,
                &sock_addr as *const ws2ipdef::SOCKADDR_INET as *const _,
                std::mem::size_of_val(&sock_addr) as i32,
            )
        }
    {
        eprintln!("Can not bind");
        let error = Err(wsa_last_error());
        let _ = unsafe { winsock2::closesocket(raw_socket) };
        return error;
    }

    let rust_sock: std::net::UdpSocket = unsafe { FromRawSocket::from_raw_socket(raw_socket as _) };
    Ok(rust_sock)
}

/// A helper for constructing an [`Endpoint`].
///
/// See [`ClientConfigBuilder`] for details on trust defaults.
///
/// [`Endpoint`]: crate::generic::Endpoint
/// [`ClientConfigBuilder`]: crate::generic::ClientConfigBuilder
#[derive(Clone, Debug)]
pub struct EndpointBuilder<S>
where
    S: proto::crypto::Session,
{
    server_config: Option<ServerConfig<S>>,
    config: EndpointConfig<S>,
    default_client_config: ClientConfig<S>,
}

#[allow(missing_docs)]
impl<S> EndpointBuilder<S>
where
    S: proto::crypto::Session + Send + 'static,
{
    /// Start a builder with a specific initial low-level configuration.
    pub fn new(config: EndpointConfig<S>, default_client_config: ClientConfig<S>) -> Self {
        Self {
            server_config: None,
            config,
            default_client_config,
        }
    }

    /// Build an endpoint bound to `addr`
    ///
    /// Must be called from within a tokio runtime context. To avoid consuming the
    /// `EndpointBuilder`, call `clone()` first.
    ///
    /// Platform defaults for dual-stack sockets vary. For example, any socket bound to a wildcard
    /// IPv6 address on Windows will not by default be able to communicate with IPv4
    /// addresses. Portable applications should bind an address that matches the family they wish to
    /// communicate within.
    pub fn bind(self, addr: &SocketAddr) -> Result<(Endpoint<S>, Incoming<S>), EndpointError> {
        let socket = create_wsa_socket(addr).map_err(|e| EndpointError::Socket(e))?;

        self.with_socket(socket)
    }

    /// Build an endpoint around a pre-configured socket
    ///
    /// Must be called from within a tokio runtime context. To avoid consuming the
    /// `EndpointBuilder`, call `clone()` first.
    pub fn with_socket(
        self,
        socket: std::net::UdpSocket,
    ) -> Result<(Endpoint<S>, Incoming<S>), EndpointError> {
        let addr = socket.local_addr().map_err(EndpointError::Socket)?;
        eprintln!("Local address is {:?}", addr);

        // let socket = UdpSocket::from_std(socket).map_err(EndpointError::Socket)?;
        let rc = EndpointRef::new(
            tokio::runtime::Handle::current(),
            socket,
            proto::generic::Endpoint::new(Arc::new(self.config), self.server_config.map(Arc::new)),
            addr.is_ipv6(),
        );

        let ev = rc.wakeup_event();

        let mut driver = EndpointDriver(rc.clone());
        std::thread::spawn(move || {
            if let Err(e) = driver.run(ev) {
                eprintln!("Endpoint error: {:?}", e);
            }
        });

        Ok((
            Endpoint {
                inner: rc.clone(),
                default_client_config: self.default_client_config,
            },
            Incoming::new(rc),
        ))
    }

    /// Accept incoming connections.
    pub fn listen(&mut self, config: ServerConfig<S>) -> &mut Self {
        self.server_config = Some(config);
        self
    }

    /// Set the default configuration used for outgoing connections.
    ///
    /// The default can be overriden by using [`Endpoint::connect_with()`].
    ///
    /// [`Endpoint::connect_with()`]: crate::generic::Endpoint::connect_with
    pub fn default_client_config(&mut self, config: ClientConfig<S>) -> &mut Self {
        self.default_client_config = config;
        self
    }
}

impl<S> Default for EndpointBuilder<S>
where
    S: proto::crypto::Session,
{
    fn default() -> Self {
        Self {
            server_config: None,
            config: EndpointConfig::default(),
            default_client_config: ClientConfig::default(),
        }
    }
}

/// Errors that can occur during the construction of an `Endpoint`.
#[derive(Debug, Error)]
pub enum EndpointError {
    /// An error during setup of the underlying UDP socket.
    #[error("failed to set up UDP socket: {0}")]
    Socket(io::Error),
}

/// Helper for constructing a [`ServerConfig`] to be passed to [`EndpointBuilder::listen()`] to
/// enable incoming connections.
///
/// [`ServerConfig`]: crate::generic::ServerConfig
/// [`EndpointBuilder::listen()`]: crate::generic::EndpointBuilder::listen
pub struct ServerConfigBuilder<S>
where
    S: proto::crypto::Session,
{
    config: ServerConfig<S>,
}

impl<S> ServerConfigBuilder<S>
where
    S: proto::crypto::Session,
{
    /// Construct a builder using `config` as the initial state.
    pub fn new(config: ServerConfig<S>) -> Self {
        Self { config }
    }

    /// Construct the complete `ServerConfig`.
    pub fn build(self) -> ServerConfig<S> {
        self.config
    }

    /// Whether to require clients to prove they can receive packets before accepting a connection
    pub fn use_stateless_retry(&mut self, enabled: bool) -> &mut Self {
        self.config.use_stateless_retry(enabled);
        self
    }
}

#[cfg(feature = "rustls")]
impl ServerConfigBuilder<proto::crypto::rustls::TlsSession> {
    /// Enable NSS-compatible cryptographic key logging to the `SSLKEYLOGFILE` environment variable.
    ///
    /// Useful for debugging encrypted communications with protocol analyzers such as Wireshark.
    pub fn enable_keylog(&mut self) -> &mut Self {
        Arc::make_mut(&mut self.config.crypto).key_log = Arc::new(rustls::KeyLogFile::new());
        self
    }

    /// Set the certificate chain that will be presented to clients.
    pub fn certificate(
        &mut self,
        cert_chain: CertificateChain,
        key: PrivateKey,
    ) -> Result<&mut Self, rustls::TLSError> {
        self.config.certificate(cert_chain, key)?;
        Ok(self)
    }

    /// Set the application-layer protocols to accept, in order of descending preference.
    ///
    /// When set, clients which don't declare support for at least one of the supplied protocols will be rejected.
    ///
    /// The IANA maintains a [registry] of standard protocol IDs, but custom IDs may be used as well.
    ///
    /// [registry]: https://www.iana.org/assignments/tls-extensiontype-values/tls-extensiontype-values.xhtml#alpn-protocol-ids
    pub fn protocols(&mut self, protocols: &[&[u8]]) -> &mut Self {
        Arc::make_mut(&mut self.config.crypto).alpn_protocols =
            protocols.iter().map(|x| x.to_vec()).collect();
        self
    }
}

impl<S> Clone for ServerConfigBuilder<S>
where
    S: proto::crypto::Session,
{
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
        }
    }
}

impl<S> Default for ServerConfigBuilder<S>
where
    S: proto::crypto::Session,
{
    fn default() -> Self {
        Self {
            config: ServerConfig::default(),
        }
    }
}

/// Helper for creating new outgoing connections.
///
/// If the `native-certs` and `ct-logs` features are enabled, [`ClientConfigBuilder::default()`] will
/// construct a configuration that trusts the host OS certificate store and uses built-in
/// certificate transparency logs respectively. These features are both enabled by default.
///
/// [`ClientConfigBuilder::default()`]: #method.default
pub struct ClientConfigBuilder<S>
where
    S: proto::crypto::Session,
{
    config: ClientConfig<S>,
}

impl<S> ClientConfigBuilder<S>
where
    S: proto::crypto::Session,
{
    /// Construct a builder using `config` as the initial state.
    ///
    /// If you want to trust the usual certificate authorities trusted by the system, use
    /// [`ClientConfigBuilder::default()`] with the `native-certs` and `ct-logs` features enabled
    /// instead.
    ///
    /// The `ClientConfigBuilder` provides a number of shortcuts to customize the TLS client
    /// behavior. However, if you want to take full control over the client's behavior (such as
    /// setting up TLS mutual authentication), you can use the associated [`new()`] function to
    /// provide a [`ClientConfig`] with TLS configuration provided directly through its `crypto`
    /// field).
    ///
    /// [`ClientConfigBuilder::default()`]: #method.default
    /// [`new()`]: ClientConfigBuilder::new
    /// [`ClientConfig`]: crate::generic::ClientConfig
    pub fn new(config: ClientConfig<S>) -> Self {
        Self { config }
    }

    /// Consume the builder and return the [`ClientConfig`], which can then be used to configure
    /// outgoing connections from an [`Endpoint`].
    ///
    /// [`ClientConfig`]: crate::generic::ClientConfig
    /// [`Endpoint`]: crate::generic::Endpoint
    pub fn build(self) -> ClientConfig<S> {
        self.config
    }
}

#[cfg(feature = "rustls")]
impl ClientConfigBuilder<proto::crypto::rustls::TlsSession> {
    /// Add a trusted certificate authority.
    ///
    /// For more advanced/less secure certificate verification, construct a [`ClientConfig`]
    /// manually and use rustls's `dangerous_configuration` feature to override the certificate
    /// verifier.
    ///
    /// [`ClientConfig`]: crate::generic::ClientConfig
    pub fn add_certificate_authority(
        &mut self,
        cert: Certificate,
    ) -> Result<&mut Self, webpki::Error> {
        self.config.add_certificate_authority(cert)?;
        Ok(self)
    }

    /// Enable NSS-compatible cryptographic key logging to the `SSLKEYLOGFILE` environment variable.
    ///
    /// Useful for debugging encrypted communications with protocol analyzers such as Wireshark.
    pub fn enable_keylog(&mut self) -> &mut Self {
        Arc::make_mut(&mut self.config.crypto).key_log = Arc::new(rustls::KeyLogFile::new());
        self
    }

    /// Set the application-layer protocols to accept, in order of descending preference.
    ///
    /// When set, clients which don't declare support for at least one of the supplied protocols will be rejected.
    ///
    /// The IANA maintains a [registry] of standard protocol IDs, but custom IDs may be used as well.
    ///
    /// [registry]: https://www.iana.org/assignments/tls-extensiontype-values/tls-extensiontype-values.xhtml#alpn-protocol-ids
    pub fn protocols(&mut self, protocols: &[&[u8]]) -> &mut Self {
        Arc::make_mut(&mut self.config.crypto).alpn_protocols =
            protocols.iter().map(|x| x.to_vec()).collect();
        self
    }

    /// Enable 0-RTT.
    pub fn enable_0rtt(&mut self) -> &mut Self {
        Arc::make_mut(&mut self.config.crypto).enable_early_data = true;
        self
    }
}

impl<S> Clone for ClientConfigBuilder<S>
where
    S: proto::crypto::Session,
{
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
        }
    }
}

impl<S> Default for ClientConfigBuilder<S>
where
    S: proto::crypto::Session,
{
    fn default() -> Self {
        Self::new(ClientConfig::default())
    }
}
