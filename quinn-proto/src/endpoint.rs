use std::{
    collections::{HashMap, VecDeque},
    convert::TryFrom,
    fmt, iter,
    net::{IpAddr, SocketAddr},
    ops::{Index, IndexMut},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use bytes::{BufMut, Bytes, BytesMut};
use fxhash::FxHashMap;
use rand::{rngs::StdRng, Rng, RngCore, SeedableRng};
use slab::Slab;
use thiserror::Error;
use tracing::{debug, trace, warn};

use crate::{
    cid_generator::{ConnectionIdGenerator, RandomConnectionIdGenerator},
    coding::BufMutExt,
    config::{ClientConfig, ConfigError, EndpointConfig, ServerConfig},
    connection::{Connection, ConnectionError},
    crypto::{
        self, ClientConfig as ClientCryptoConfig, Keys, PacketKey,
        ServerConfig as ServerCryptoConfig,
    },
    frame,
    packet::{Header, Packet, PacketDecodeError, PacketNumber, PartialDecode},
    shared::{
        ConnectionEvent, ConnectionEventInner, ConnectionId, EcnCodepoint, EndpointEvent,
        EndpointEventInner, IssuedCid,
    },
    transport_parameters::TransportParameters,
    ResetToken, RetryToken, Side, Transmit, TransportError, INITIAL_MAX_UDP_PAYLOAD_SIZE,
    MAX_CID_SIZE, MIN_INITIAL_SIZE, RESET_TOKEN_SIZE,
};

/// The main entry point to the library
///
/// This object performs no I/O whatsoever. Instead, it generates a stream of packets to send via
/// `poll_transmit`, and consumes incoming packets and connection-generated events via `handle` and
/// `handle_event`.
pub struct Endpoint<S>
where
    S: crypto::Session,
{
    rng: StdRng,
    transmits: VecDeque<Transmit>,
    /// Identifies connections based on the initial DCID the peer utilized
    ///
    /// Uses a standard `HashMap` to protect against hash collision attacks.
    connection_ids_initial: HashMap<ConnectionId, ConnectionHandle>,
    /// Identifies connections based on locally created CIDs
    ///
    /// Uses a cheaper hash function since keys are locally created
    connection_ids: FxHashMap<ConnectionId, ConnectionHandle>,
    /// Identifies connections with zero-length CIDs
    ///
    /// Uses a standard `HashMap` to protect against hash collision attacks.
    connection_remotes: HashMap<SocketAddr, ConnectionHandle>,
    /// Reset tokens provided by the peer for the CID each connection is currently sending to
    ///
    /// Incoming stateless resets do not have correct CIDs, so we need this to identify the correct
    /// recipient, if any.
    connection_reset_tokens: ResetTokenTable,
    connections: Slab<ConnectionMeta>,
    local_cid_generator: Box<dyn ConnectionIdGenerator>,
    config: Arc<EndpointConfig<S>>,
    server_config: Option<Arc<ServerConfig<S>>>,
    /// Whether incoming connections should be unconditionally rejected by a server
    ///
    /// Equivalent to a `ServerConfig.accept_buffer` of `0`, but can be changed after the endpoint is constructed.
    reject_new_connections: bool,
}

impl<S> Endpoint<S>
where
    S: crypto::Session,
{
    /// Create a new endpoint
    ///
    /// Returns `Err` if the configuration is invalid.
    pub fn new(
        config: Arc<EndpointConfig<S>>,
        server_config: Option<Arc<ServerConfig<S>>>,
    ) -> Self {
        Self {
            rng: StdRng::from_entropy(),
            transmits: VecDeque::new(),
            connection_ids_initial: HashMap::default(),
            connection_ids: FxHashMap::default(),
            connection_remotes: HashMap::default(),
            connection_reset_tokens: ResetTokenTable::default(),
            connections: Slab::new(),
            local_cid_generator: (config.connection_id_generator_factory.as_ref())(),
            reject_new_connections: false,
            config,
            server_config,
        }
    }

    fn is_server(&self) -> bool {
        self.server_config.is_some()
    }

    /// Get the next packet to transmit
    #[must_use]
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    /// Process `EndpointEvent`s emitted from related `Connection`s
    ///
    /// In turn, processing this event may return a `ConnectionEvent` for the same `Connection`.
    pub fn handle_event(
        &mut self,
        ch: ConnectionHandle,
        event: EndpointEvent,
    ) -> Option<ConnectionEvent> {
        use EndpointEventInner::*;
        match event.0 {
            NeedIdentifiers(now, n) => {
                return Some(self.send_new_identifiers(now, ch, n));
            }
            ResetToken(remote, token) => {
                if let Some(old) = self.connections[ch].reset_token.replace((remote, token)) {
                    self.connection_reset_tokens.remove(old.0, old.1);
                }
                if self.connection_reset_tokens.insert(remote, token, ch) {
                    warn!("duplicate reset token");
                }
            }
            RetireConnectionId(now, seq, allow_more_cids) => {
                if let Some(cid) = self.connections[ch].loc_cids.remove(&seq) {
                    trace!("peer retired CID {}: {}", seq, cid);
                    self.connection_ids.remove(&cid);
                    if allow_more_cids {
                        return Some(self.send_new_identifiers(now, ch, 1));
                    }
                }
            }
            Drained => {
                let conn = self.connections.remove(ch.0);
                if conn.init_cid.len() > 0 {
                    self.connection_ids_initial.remove(&conn.init_cid);
                }
                for cid in conn.loc_cids.values() {
                    self.connection_ids.remove(&cid);
                }
                self.connection_remotes.remove(&conn.initial_remote);
                if let Some((remote, token)) = conn.reset_token {
                    self.connection_reset_tokens.remove(remote, token);
                }
            }
        }
        None
    }

    /// Process an incoming UDP datagram
    pub fn handle(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        ecn: Option<EcnCodepoint>,
        data: BytesMut,
    ) -> Option<(ConnectionHandle, DatagramEvent<S>)> {
        let datagram_len = data.len();
        let (first_decode, remaining) = match PartialDecode::new(
            data,
            self.local_cid_generator.cid_len(),
            &self.config.supported_versions,
        ) {
            Ok(x) => x,
            Err(PacketDecodeError::UnsupportedVersion {
                src_cid,
                dst_cid,
                version,
            }) => {
                if !self.is_server() {
                    debug!("dropping packet with unsupported version");
                    return None;
                }
                trace!("sending version negotiation");
                // Negotiate versions
                let mut buf = Vec::<u8>::new();
                Header::VersionNegotiate {
                    random: self.rng.gen::<u8>() | 0x40,
                    src_cid: dst_cid,
                    dst_cid: src_cid,
                }
                .encode(&mut buf);
                // Grease with a reserved version
                if version != 0x0a1a_2a3a {
                    buf.write::<u32>(0x0a1a_2a3a);
                } else {
                    buf.write::<u32>(0x0a1a_2a4a);
                }
                buf.write(self.config.initial_version); // supported version
                self.transmits.push_back(Transmit {
                    destination: remote,
                    ecn: None,
                    contents: buf,
                    segment_size: None,
                    src_ip: local_ip,
                });
                return None;
            }
            Err(e) => {
                trace!("malformed header: {}", e);
                return None;
            }
        };

        //
        // Handle packet on existing connection, if any
        //

        let dst_cid = first_decode.dst_cid();
        let known_ch = {
            let ch = if self.local_cid_generator.cid_len() > 0 {
                self.connection_ids.get(&dst_cid)
            } else {
                None
            };
            ch.or_else(|| {
                if first_decode.is_initial() || first_decode.is_0rtt() {
                    self.connection_ids_initial.get(&dst_cid)
                } else {
                    None
                }
            })
            .or_else(|| {
                if self.local_cid_generator.cid_len() == 0 {
                    self.connection_remotes.get(&remote)
                } else {
                    None
                }
            })
            .or_else(|| {
                let data = first_decode.data();
                if data.len() < RESET_TOKEN_SIZE {
                    return None;
                }
                self.connection_reset_tokens
                    .get(remote, &data[data.len() - RESET_TOKEN_SIZE..])
            })
            .cloned()
        };
        if let Some(ch) = known_ch {
            return Some((
                ch,
                DatagramEvent::ConnectionEvent(ConnectionEvent(ConnectionEventInner::Datagram {
                    now,
                    remote,
                    ecn,
                    first_decode,
                    remaining,
                })),
            ));
        }

        //
        // Potentially create a new connection
        //

        if !self.is_server() {
            debug!("packet for unrecognized connection {}", dst_cid);
            self.stateless_reset(datagram_len, remote, local_ip, &dst_cid);
            return None;
        }

        if first_decode.has_long_header() {
            if !first_decode.is_initial() {
                debug!(
                    "ignoring non-initial packet for unknown connection {}",
                    dst_cid
                );
                return None;
            }
            if datagram_len < MIN_INITIAL_SIZE as usize {
                debug!("ignoring short initial for connection {}", dst_cid);
                return None;
            }

            let crypto = S::initial_keys(&dst_cid, Side::Server);
            return match first_decode.finish(Some(&crypto.header.remote)) {
                Ok(packet) => self
                    .handle_first_packet(now, remote, local_ip, ecn, packet, remaining, &crypto)
                    .map(|(ch, conn)| (ch, DatagramEvent::NewConnection(conn))),
                Err(e) => {
                    trace!("unable to decode initial packet: {}", e);
                    None
                }
            };
        }

        //
        // If we got this far, we're a server receiving a seemingly valid packet for an unknown
        // connection. Send a stateless reset.
        //

        if !dst_cid.is_empty() {
            self.stateless_reset(datagram_len, remote, local_ip, &dst_cid);
        } else {
            trace!("dropping unrecognized short packet without ID");
        }
        None
    }

    fn stateless_reset(
        &mut self,
        inciting_dgram_len: usize,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        dst_cid: &ConnectionId,
    ) {
        /// Minimum amount of padding for the stateless reset to look like a short-header packet
        const MIN_PADDING_LEN: usize = 5;

        // Prevent amplification attacks and reset loops by ensuring we pad to at most 1 byte
        // smaller than the inciting packet.
        let max_padding_len = match inciting_dgram_len.checked_sub(RESET_TOKEN_SIZE) {
            Some(headroom) if headroom > MIN_PADDING_LEN => headroom - 1,
            _ => {
                debug!("ignoring unexpected {} byte packet: not larger than minimum stateless reset size", inciting_dgram_len);
                return;
            }
        };

        debug!("sending stateless reset for {} to {}", dst_cid, remote);
        let mut buf = Vec::<u8>::new();
        // Resets with at least this much padding can't possibly be distinguished from real packets
        const IDEAL_MIN_PADDING_LEN: usize = MIN_PADDING_LEN + MAX_CID_SIZE;
        let padding_len = if max_padding_len <= IDEAL_MIN_PADDING_LEN {
            max_padding_len
        } else {
            self.rng.gen_range(IDEAL_MIN_PADDING_LEN..max_padding_len)
        };
        buf.reserve_exact(padding_len + RESET_TOKEN_SIZE);
        buf.resize(padding_len, 0);
        self.rng.fill_bytes(&mut buf[0..padding_len]);
        buf[0] = 0b0100_0000 | buf[0] >> 2;
        buf.extend_from_slice(&ResetToken::new(&*self.config.reset_key, dst_cid));

        debug_assert!(buf.len() < inciting_dgram_len);

        self.transmits.push_back(Transmit {
            destination: remote,
            ecn: None,
            contents: buf,
            segment_size: None,
            src_ip: local_ip,
        });
    }

    /// Initiate a connection
    pub fn connect(
        &mut self,
        config: ClientConfig<S>,
        remote: SocketAddr,
        server_name: &str,
    ) -> Result<(ConnectionHandle, Connection<S>), ConnectError> {
        if self.is_full() {
            return Err(ConnectError::TooManyConnections);
        }
        if remote.port() == 0 {
            return Err(ConnectError::InvalidRemoteAddress(remote));
        }
        let remote_id = RandomConnectionIdGenerator::new(MAX_CID_SIZE).generate_cid();
        trace!(initial_dcid = %remote_id);
        let (ch, conn) = self.add_connection(
            remote_id,
            remote_id,
            remote,
            None,
            ConnectionOpts::Client {
                config,
                server_name: server_name.into(),
            },
            Instant::now(),
        )?;
        Ok((ch, conn))
    }

    fn send_new_identifiers(
        &mut self,
        now: Instant,
        ch: ConnectionHandle,
        num: u64,
    ) -> ConnectionEvent {
        let mut ids = vec![];
        for _ in 0..num {
            let id = self.new_cid();
            self.connection_ids.insert(id, ch);
            let meta = &mut self.connections[ch];
            meta.cids_issued += 1;
            let sequence = meta.cids_issued;
            meta.loc_cids.insert(sequence, id);
            ids.push(IssuedCid {
                sequence,
                id,
                reset_token: ResetToken::new(&*self.config.reset_key, &id),
            });
        }
        ConnectionEvent(ConnectionEventInner::NewIdentifiers(ids, now))
    }

    fn new_cid(&mut self) -> ConnectionId {
        loop {
            let cid = self.local_cid_generator.generate_cid();
            if !self.connection_ids.contains_key(&cid) {
                break cid;
            }
            assert!(self.local_cid_generator.cid_len() > 0);
        }
    }

    fn add_connection(
        &mut self,
        init_cid: ConnectionId,
        rem_cid: ConnectionId,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        opts: ConnectionOpts<S>,
        now: Instant,
    ) -> Result<(ConnectionHandle, Connection<S>), ConnectError> {
        let loc_cid = self.new_cid();
        let (server_config, tls, transport_config) = match opts {
            ConnectionOpts::Client {
                config,
                server_name,
            } => {
                let params = TransportParameters::new::<S>(
                    &config.transport,
                    &self.config,
                    self.local_cid_generator.as_ref(),
                    loc_cid,
                    None,
                );
                (
                    None,
                    config.crypto.start_session(&server_name, &params)?,
                    config.transport,
                )
            }
            ConnectionOpts::Server {
                orig_dst_cid,
                retry_src_cid,
            } => {
                let config = self.server_config.as_ref().unwrap();
                let params = TransportParameters::new(
                    &config.transport,
                    &self.config,
                    self.local_cid_generator.as_ref(),
                    loc_cid,
                    Some(config),
                );
                let server_params = TransportParameters {
                    stateless_reset_token: Some(ResetToken::new(&*self.config.reset_key, &loc_cid)),
                    original_dst_cid: Some(orig_dst_cid),
                    retry_src_cid,
                    ..params
                };
                (
                    Some(config.clone()),
                    config.crypto.start_session(&server_params),
                    config.transport.clone(),
                )
            }
        };

        let conn = Connection::new(
            server_config,
            transport_config,
            init_cid,
            loc_cid,
            rem_cid,
            remote,
            local_ip,
            tls,
            self.local_cid_generator.as_ref(),
            now,
            self.config.initial_version,
        );
        let id = self.connections.insert(ConnectionMeta {
            init_cid,
            cids_issued: 0,
            loc_cids: iter::once((0, loc_cid)).collect(),
            initial_remote: remote,
            reset_token: None,
        });
        let ch = ConnectionHandle(id);

        if self.local_cid_generator.cid_len() > 0 {
            self.connection_ids.insert(loc_cid, ch);
        } else {
            self.connection_remotes.insert(remote, ch);
        }
        Ok((ch, conn))
    }

    fn handle_first_packet(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        ecn: Option<EcnCodepoint>,
        mut packet: Packet,
        rest: Option<BytesMut>,
        crypto: &Keys<S>,
    ) -> Option<(ConnectionHandle, Connection<S>)> {
        let (src_cid, dst_cid, token, packet_number) = match packet.header {
            Header::Initial {
                src_cid,
                dst_cid,
                ref token,
                number,
                ..
            } => (src_cid, dst_cid, token.clone(), number),
            _ => panic!("non-initial packet in handle_first_packet()"),
        };
        let packet_number = packet_number.expand(0);

        if crypto
            .packet
            .remote
            .decrypt(
                packet_number as u64,
                &packet.header_data,
                &mut packet.payload,
            )
            .is_err()
        {
            debug!(packet_number, "failed to authenticate initial packet");
            return None;
        };

        if !packet.reserved_bits_valid() {
            debug!("dropping connection attempt with invalid reserved bits");
            return None;
        }

        // Local CID used for stateless packets
        let temp_loc_cid = self.new_cid();
        let server_config = self.server_config.as_ref().unwrap();

        if self.connections.len() >= server_config.concurrent_connections as usize
            || self.reject_new_connections
            || self.is_full()
        {
            debug!("refusing connection");
            self.initial_close(
                remote,
                local_ip,
                crypto,
                &src_cid,
                &temp_loc_cid,
                TransportError::CONNECTION_REFUSED(""),
            );
            return None;
        }

        if dst_cid.len() < 8
            && (!server_config.use_stateless_retry
                || dst_cid.len() != self.local_cid_generator.cid_len())
        {
            debug!(
                "rejecting connection due to invalid DCID length {}",
                dst_cid.len()
            );
            self.initial_close(
                remote,
                local_ip,
                crypto,
                &src_cid,
                &temp_loc_cid,
                TransportError::PROTOCOL_VIOLATION("invalid destination CID length"),
            );
            return None;
        }

        let (retry_src_cid, orig_dst_cid) = if server_config.use_stateless_retry {
            if token.is_empty() {
                // First Initial
                let mut random_bytes = vec![0u8; RetryToken::RANDOM_BYTES_LEN];
                self.rng.fill_bytes(&mut random_bytes);

                let token = RetryToken {
                    orig_dst_cid: dst_cid,
                    issued: SystemTime::now(),
                    random_bytes: &random_bytes,
                }
                .encode(&*server_config.token_key, &remote, &temp_loc_cid);

                let header = Header::Retry {
                    src_cid: temp_loc_cid,
                    dst_cid: src_cid,
                    version: self.config.initial_version,
                };

                let mut buf = Vec::new();
                let encode = header.encode(&mut buf);
                buf.put_slice(&token);
                buf.extend_from_slice(&S::retry_tag(&dst_cid, &buf));
                encode.finish::<S::PacketKey, S::HeaderKey>(&mut buf, &crypto.header.local, None);

                self.transmits.push_back(Transmit {
                    destination: remote,
                    ecn: None,
                    contents: buf,
                    segment_size: None,
                    src_ip: local_ip,
                });
                return None;
            }

            match RetryToken::from_bytes(&*server_config.token_key, &remote, &dst_cid, &token) {
                Ok(token)
                    if token.issued
                        + Duration::from_micros(
                            self.server_config.as_ref().unwrap().retry_token_lifetime,
                        )
                        > SystemTime::now() =>
                {
                    (Some(dst_cid), token.orig_dst_cid)
                }
                _ => {
                    debug!("rejecting invalid stateless retry token");
                    self.initial_close(
                        remote,
                        local_ip,
                        crypto,
                        &src_cid,
                        &temp_loc_cid,
                        TransportError::INVALID_TOKEN(""),
                    );
                    return None;
                }
            }
        } else {
            (None, dst_cid)
        };

        let (ch, mut conn) = self
            .add_connection(
                dst_cid,
                src_cid,
                remote,
                local_ip,
                ConnectionOpts::Server {
                    retry_src_cid,
                    orig_dst_cid,
                },
                now,
            )
            .unwrap();
        if dst_cid.len() != 0 {
            self.connection_ids_initial.insert(dst_cid, ch);
        }
        match conn.handle_first_packet(now, remote, ecn, packet_number as u64, packet, rest) {
            Ok(()) => {
                trace!(id = ch.0, icid = %dst_cid, "connection incoming");
                Some((ch, conn))
            }
            Err(e) => {
                debug!("handshake failed: {}", e);
                self.handle_event(ch, EndpointEvent(EndpointEventInner::Drained));
                if let ConnectionError::TransportError(e) = e {
                    self.initial_close(remote, local_ip, crypto, &src_cid, &temp_loc_cid, e);
                }
                None
            }
        }
    }

    fn initial_close(
        &mut self,
        destination: SocketAddr,
        local_ip: Option<IpAddr>,
        crypto: &Keys<S>,
        remote_id: &ConnectionId,
        local_id: &ConnectionId,
        reason: TransportError,
    ) {
        let number = PacketNumber::U8(0);
        let header = Header::Initial {
            dst_cid: *remote_id,
            src_cid: *local_id,
            number,
            token: Bytes::new(),
            version: self.config.initial_version,
        };

        let mut buf = Vec::<u8>::new();
        let partial_encode = header.encode(&mut buf);
        let max_len = INITIAL_MAX_UDP_PAYLOAD_SIZE as usize
            - partial_encode.header_len
            - crypto.packet.local.tag_len();
        frame::Close::from(reason).encode(&mut buf, max_len);
        buf.resize(buf.len() + crypto.packet.local.tag_len(), 0);
        partial_encode.finish(
            &mut buf,
            &crypto.header.local,
            Some((0, &crypto.packet.local)),
        );
        self.transmits.push_back(Transmit {
            destination,
            ecn: None,
            contents: buf,
            segment_size: None,
            src_ip: local_ip,
        })
    }

    /// Unconditionally reject future incoming connections
    pub fn reject_new_connections(&mut self) {
        self.reject_new_connections = true;
    }

    /// Access the configuration used by this endpoint
    pub fn config(&self) -> &EndpointConfig<S> {
        &self.config
    }

    #[cfg(test)]
    pub(crate) fn known_connections(&self) -> usize {
        let x = self.connections.len();
        debug_assert_eq!(x, self.connection_ids_initial.len());
        // Not all connections have known reset tokens
        debug_assert!(x >= self.connection_reset_tokens.0.len());
        // Not all connections have unique remotes, and 0-length CIDs might not be in use.
        debug_assert!(x >= self.connection_remotes.len());
        x
    }

    #[cfg(test)]
    pub(crate) fn known_cids(&self) -> usize {
        self.connection_ids.len()
    }

    /// Whether we've used up 3/4 of the available CID space
    ///
    /// We leave some space unused so that `new_cid` can be relied upon to finish quickly. We don't
    /// bother to check when CID longer than 4 bytes are used because 2^40 connections is a lot.
    fn is_full(&self) -> bool {
        self.local_cid_generator.cid_len() <= 4
            && self.local_cid_generator.cid_len() != 0
            && (2usize.pow(self.local_cid_generator.cid_len() as u32 * 8)
                - self.connection_ids.len())
                < 2usize.pow(self.local_cid_generator.cid_len() as u32 * 8 - 2)
    }
}

impl<S> fmt::Debug for Endpoint<S>
where
    S: crypto::Session,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Endpoint<T>")
            .field("rng", &self.rng)
            .field("transmits", &self.transmits)
            .field("connection_ids_initial", &self.connection_ids_initial)
            .field("connection_ids", &self.connection_ids)
            .field("connection_remotes", &self.connection_remotes)
            .field("connection_reset_tokens", &self.connection_reset_tokens)
            .field("connections", &self.connections)
            .field("config", &self.config)
            .field("server_config", &self.server_config)
            .field("reject_new_connections", &self.reject_new_connections)
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct ConnectionMeta {
    init_cid: ConnectionId,
    /// Number of local connection IDs that have been issued in NEW_CONNECTION_ID frames.
    cids_issued: u64,
    loc_cids: FxHashMap<u64, ConnectionId>,
    /// Remote address the connection began with
    ///
    /// Only needed to support connections with zero-length CIDs, which cannot migrate, so we don't
    /// bother keeping it up to date.
    initial_remote: SocketAddr,
    /// Reset token provided by the peer for the CID we're currently sending to, and the address
    /// being sent to
    reset_token: Option<(SocketAddr, ResetToken)>,
}

/// Internal identifier for a `Connection` currently associated with an endpoint
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ConnectionHandle(pub usize);

impl From<ConnectionHandle> for usize {
    fn from(x: ConnectionHandle) -> usize {
        x.0
    }
}

impl Index<ConnectionHandle> for Slab<ConnectionMeta> {
    type Output = ConnectionMeta;
    fn index(&self, ch: ConnectionHandle) -> &ConnectionMeta {
        &self[ch.0]
    }
}

impl IndexMut<ConnectionHandle> for Slab<ConnectionMeta> {
    fn index_mut(&mut self, ch: ConnectionHandle) -> &mut ConnectionMeta {
        &mut self[ch.0]
    }
}

/// Event resulting from processing a single datagram
pub enum DatagramEvent<S>
where
    S: crypto::Session,
{
    /// The datagram is redirected to its `Connection`
    ConnectionEvent(ConnectionEvent),
    /// The datagram has resulted in starting a new `Connection`
    NewConnection(Connection<S>),
}

enum ConnectionOpts<S: crypto::Session> {
    Client {
        config: ClientConfig<S>,
        server_name: String,
    },
    Server {
        retry_src_cid: Option<ConnectionId>,
        orig_dst_cid: ConnectionId,
    },
}

/// Errors in the parameters being used to create a new connection
///
/// These arise before any I/O has been performed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The endpoint can no longer create new connections
    ///
    /// Indicates that a necessary component of the endpoint has been dropped or otherwise disabled.
    #[error("endpoint stopping")]
    EndpointStopping,
    /// The number of active connections on the local endpoint is at the limit
    ///
    /// Try using longer connection IDs.
    #[error("too many connections")]
    TooManyConnections,
    /// The domain name supplied was malformed
    #[error("invalid DNS name: {0}")]
    InvalidDnsName(String),
    /// The transport configuration was invalid
    #[error("transport configuration error: {0}")]
    Config(#[source] ConfigError),
    /// The remote [`SocketAddr`] supplied was malformed
    ///
    /// Examples include attempting to connect to port 0, or using an inappropriate address family.
    #[error("invalid remote address: {0}")]
    InvalidRemoteAddress(SocketAddr),
    /// No default client configuration was set up
    ///
    /// Use [`Endpoint::connect_with`] to specify a client configuration.
    #[error("no default client config")]
    NoDefaultClientConfig,
}

/// Reset Tokens which are associated with peer socket addresses
///
/// The standard `HashMap` is used since both `SocketAddr` and `ResetToken` are
/// peer generated and might be usable for hash collision attacks.
#[derive(Default, Debug)]
struct ResetTokenTable(HashMap<SocketAddr, HashMap<ResetToken, ConnectionHandle>>);

impl ResetTokenTable {
    fn insert(&mut self, remote: SocketAddr, token: ResetToken, ch: ConnectionHandle) -> bool {
        self.0
            .entry(remote)
            .or_default()
            .insert(token, ch)
            .is_some()
    }

    fn remove(&mut self, remote: SocketAddr, token: ResetToken) {
        use std::collections::hash_map::Entry;
        match self.0.entry(remote) {
            Entry::Vacant(_) => {}
            Entry::Occupied(mut e) => {
                e.get_mut().remove(&token);
                if e.get().is_empty() {
                    e.remove_entry();
                }
            }
        }
    }

    fn get(&self, remote: SocketAddr, token: &[u8]) -> Option<&ConnectionHandle> {
        let token = ResetToken::from(<[u8; RESET_TOKEN_SIZE]>::try_from(token).ok()?);
        self.0.get(&remote)?.get(&token)
    }
}
