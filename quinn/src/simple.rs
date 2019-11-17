//! Simplified interface
//!
//! This module makes it easy to get a prototype up and running quickly. Real-world applications are
//! encouraged to use the regular interface to optimize for the application protocol in question.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use err_derive::Error;
use futures::{future, FutureExt, Stream, StreamExt};

use crate::{
    CertificateChain, ConnectError, Connection, ConnectionError, Endpoint, EndpointError,
    IncomingStreams, PrivateKey,
};

/// Connect to a server at `remote`, authenticating it as `domain_name`
///
/// `domain_name` must be a valid DNS name. If the server cannot authenticate itself under that name
/// with a valid TLS certificate, the connection will fail.
///
/// # Example
/// ```no_run
/// # async {
/// use std::net::ToSocketAddrs;
/// let domain_name = "example.com";
/// let addr = domain_name.to_socket_addrs().unwrap().next().unwrap();
/// let nc = quinn::simple::connect(&addr, domain_name).await.unwrap();
/// # };
/// ```
pub async fn connect(remote: &SocketAddr, domain_name: &str) -> Result<NewConnection, SimpleError> {
    let bind_addr: SocketAddr = match remote {
        SocketAddr::V4(_) => SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0).into(),
    };
    let (endpoint_driver, endpoint, _) = Endpoint::builder().bind(bind_addr)?;
    tokio_executor::spawn(endpoint_driver.map(|_| ()));
    let conn = endpoint
        .connect(&remote, domain_name)
        .map_err(|e| match e {
            ConnectError::EndpointStopping | ConnectError::Config(_) => {
                unreachable!("impossible under the simple connect interface")
            }
            ConnectError::InvalidDnsName(_) => SimpleError::InvalidDnsName,
        })?
        .await?;
    Ok(NewConnection::new(conn))
}

/// Listen for incoming connections on `address`
///
/// # Example
/// ```no_run
/// # async {
/// use std::net::{SocketAddr, Ipv6Addr};
/// let certs = quinn::CertificateChain::from_pem(&std::fs::read("fullchain.pem").unwrap()).unwrap();
/// let key = quinn::PrivateKey::from_pem(&std::fs::read("key.pem").unwrap()).unwrap();
/// let mut incoming = quinn::simple::listen(&"[::]:0".parse().unwrap(), certs, key).unwrap();
/// use futures::StreamExt;
/// while let Some(connection) = incoming.next().await {
///   // ...
/// }
/// # };
/// ```
pub fn listen(
    address: &SocketAddr,
    certificate_chain: CertificateChain,
    private_key: PrivateKey,
) -> Result<impl Stream<Item = NewConnection>, SimpleError> {
    let mut server_config = crate::ServerConfigBuilder::default();
    server_config
        .certificate(certificate_chain, private_key)
        .map_err(EndpointError::Tls)?;
    let mut endpoint = Endpoint::builder();
    endpoint.listen(server_config.build());
    let (endpoint_driver, _, conns) = endpoint.bind(address)?;
    tokio_executor::spawn(endpoint_driver.map(|_| ()));
    Ok(conns
        .buffer_unordered(4096)
        .filter_map(|conn| future::ready(conn.ok().map(NewConnection::new))))
}

/// Components of a newly established simple connection
pub struct NewConnection {
    /// Handle for interacting with the connection
    pub connection: Connection,
    /// Streams initiated by the peer, in the order they were opened
    pub streams: IncomingStreams,
    /// Leave room for future extensions
    _non_exhaustive: (),
}

impl NewConnection {
    fn new(x: crate::NewConnection) -> Self {
        let crate::NewConnection {
            driver,
            connection,
            streams,
            _non_exhaustive: (),
        } = x;
        tokio_executor::spawn(driver.map(|_| ()));
        Self {
            connection,
            streams,
            _non_exhaustive: (),
        }
    }
}

/// Errors that may arise from from functions in this module
#[derive(Debug, Error)]
pub enum SimpleError {
    /// Endpoint setup failed
    #[error(display = "{}", 0)]
    EndpointError(EndpointError),
    /// The DNS name supplied to authenticate the peer is invalid
    #[error(display = "invalid DNS name")]
    InvalidDnsName,
    /// The connection failed
    #[error(display = "connection failed: {}", 0)]
    ConnectionError(ConnectionError),
}

impl From<ConnectionError> for SimpleError {
    fn from(x: ConnectionError) -> Self {
        Self::ConnectionError(x)
    }
}

impl From<EndpointError> for SimpleError {
    fn from(x: EndpointError) -> Self {
        Self::EndpointError(x)
    }
}
