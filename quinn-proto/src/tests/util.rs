use std::{
    cmp,
    collections::{HashMap, VecDeque},
    env,
    io::{self, Write},
    mem,
    net::{Ipv6Addr, SocketAddr, UdpSocket},
    ops::RangeFrom,
    str,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use assert_matches::assert_matches;
use lazy_static::lazy_static;
use rustls::KeyLogFile;
use tracing::{info_span, trace};

use super::*;

pub struct Pair {
    pub server: TestEndpoint,
    pub client: TestEndpoint,
    pub time: Instant,
    // One-way
    pub latency: Duration,
    /// Number of spin bit flips
    pub spins: u64,
    last_spin: bool,
}

impl Pair {
    pub fn new(endpoint_config: Arc<EndpointConfig>, server_config: ServerConfig) -> Self {
        let server = Endpoint::new(endpoint_config.clone(), Some(Arc::new(server_config)));
        let client = Endpoint::new(endpoint_config, None);

        Pair::new_from_endpoint(client, server)
    }

    pub fn new_from_endpoint(client: Endpoint, server: Endpoint) -> Self {
        let server_addr = SocketAddr::new(
            Ipv6Addr::LOCALHOST.into(),
            SERVER_PORTS.lock().unwrap().next().unwrap(),
        );
        let client_addr = SocketAddr::new(
            Ipv6Addr::LOCALHOST.into(),
            CLIENT_PORTS.lock().unwrap().next().unwrap(),
        );
        Self {
            server: TestEndpoint::new(server, server_addr),
            client: TestEndpoint::new(client, client_addr),
            time: Instant::now(),
            latency: Duration::new(0, 0),
            spins: 0,
            last_spin: false,
        }
    }

    /// Returns whether the connection is not idle
    pub fn step(&mut self) -> bool {
        self.drive_client();
        self.drive_server();
        if self.client.is_idle() && self.server.is_idle() {
            return false;
        }

        let client_t = self.client.next_wakeup();
        let server_t = self.server.next_wakeup();
        match min_opt(client_t, server_t) {
            Some(t) if Some(t) == client_t => {
                if t != self.time {
                    self.time = self.time.max(t);
                    trace!("advancing to {:?} for client", self.time);
                }
                true
            }
            Some(t) if Some(t) == server_t => {
                if t != self.time {
                    self.time = self.time.max(t);
                    trace!("advancing to {:?} for server", self.time);
                }
                true
            }
            Some(_) => unreachable!(),
            None => false,
        }
    }

    /// Advance time until both connections are idle
    pub fn drive(&mut self) {
        while self.step() {}
    }

    pub fn drive_client(&mut self) {
        let span = info_span!("client");
        let _guard = span.enter();
        self.client.drive(self.time, self.server.addr);
        for x in self.client.outbound.drain(..) {
            if x.contents[0] & packet::LONG_HEADER_FORM == 0 {
                let spin = x.contents[0] & packet::SPIN_BIT != 0;
                self.spins += (spin == self.last_spin) as u64;
                self.last_spin = spin;
            }
            if let Some(ref socket) = self.client.socket {
                socket.send_to(&x.contents, x.destination).unwrap();
            }
            if self.server.addr == x.destination {
                self.server
                    .inbound
                    .push_back((self.time + self.latency, x.ecn, x.contents));
            }
        }
    }

    pub fn drive_server(&mut self) {
        let span = info_span!("server");
        let _guard = span.enter();
        self.server.drive(self.time, self.client.addr);
        for x in self.server.outbound.drain(..) {
            if let Some(ref socket) = self.server.socket {
                socket.send_to(&x.contents, x.destination).unwrap();
            }
            if self.client.addr == x.destination {
                self.client
                    .inbound
                    .push_back((self.time + self.latency, x.ecn, x.contents));
            }
        }
    }

    pub fn connect(&mut self) -> (ConnectionHandle, ConnectionHandle) {
        info!("connecting");
        let client_ch = self.begin_connect(client_config(None));
        self.drive();
        let server_ch = self.server.assert_accept();
        assert_matches!(
            self.client_conn_mut(client_ch).poll(),
            Some(Event::HandshakeDataReady)
        );
        assert_matches!(
            self.client_conn_mut(client_ch).poll(),
            Some(Event::Connected { .. })
        );
        assert_matches!(
            self.server_conn_mut(server_ch).poll(),
            Some(Event::HandshakeDataReady)
        );
        assert_matches!(
            self.server_conn_mut(server_ch).poll(),
            Some(Event::Connected { .. })
        );
        (client_ch, server_ch)
    }

    /// Just start connecting the client
    pub fn begin_connect(&mut self, config: ClientConfig) -> ConnectionHandle {
        let span = info_span!("client");
        let _guard = span.enter();
        let (client_ch, client_conn) = self
            .client
            .connect(config, self.server.addr, "localhost")
            .unwrap();
        self.client.connections.insert(client_ch, client_conn);
        client_ch
    }

    pub fn client_conn_mut(&mut self, ch: ConnectionHandle) -> &mut Connection {
        self.client.connections.get_mut(&ch).unwrap()
    }

    pub fn client_streams(&mut self, ch: ConnectionHandle) -> Streams<'_> {
        self.client_conn_mut(ch).streams()
    }

    pub fn client_send(&mut self, ch: ConnectionHandle, s: StreamId) -> SendStream<'_> {
        self.client_conn_mut(ch).send_stream(s)
    }

    pub fn client_recv(&mut self, ch: ConnectionHandle, s: StreamId) -> RecvStream<'_> {
        self.client_conn_mut(ch).recv_stream(s)
    }

    pub fn client_datagrams(&mut self, ch: ConnectionHandle) -> Datagrams<'_> {
        self.client_conn_mut(ch).datagrams()
    }

    pub fn server_conn_mut(&mut self, ch: ConnectionHandle) -> &mut Connection {
        self.server.connections.get_mut(&ch).unwrap()
    }

    pub fn server_streams(&mut self, ch: ConnectionHandle) -> Streams<'_> {
        self.server_conn_mut(ch).streams()
    }

    pub fn server_send(&mut self, ch: ConnectionHandle, s: StreamId) -> SendStream<'_> {
        self.server_conn_mut(ch).send_stream(s)
    }

    pub fn server_recv(&mut self, ch: ConnectionHandle, s: StreamId) -> RecvStream<'_> {
        self.server_conn_mut(ch).recv_stream(s)
    }

    pub fn server_datagrams(&mut self, ch: ConnectionHandle) -> Datagrams<'_> {
        self.server_conn_mut(ch).datagrams()
    }
}

impl Default for Pair {
    fn default() -> Self {
        Pair::new(Default::default(), server_config(None))
    }
}

pub struct TestEndpoint {
    pub endpoint: Endpoint,
    pub addr: SocketAddr,
    socket: Option<UdpSocket>,
    timeout: Option<Instant>,
    pub outbound: VecDeque<Transmit>,
    delayed: VecDeque<Transmit>,
    pub inbound: VecDeque<(Instant, Option<EcnCodepoint>, Vec<u8>)>,
    accepted: Option<ConnectionHandle>,
    pub connections: HashMap<ConnectionHandle, Connection>,
    conn_events: HashMap<ConnectionHandle, VecDeque<ConnectionEvent>>,
}

impl TestEndpoint {
    fn new(endpoint: Endpoint, addr: SocketAddr) -> Self {
        let socket = if env::var_os("SSLKEYLOGFILE").is_some() {
            let socket = UdpSocket::bind(addr).expect("failed to bind UDP socket");
            socket
                .set_read_timeout(Some(Duration::new(0, 10_000_000)))
                .unwrap();
            Some(socket)
        } else {
            None
        };
        Self {
            endpoint,
            addr,
            socket,
            timeout: None,
            outbound: VecDeque::new(),
            delayed: VecDeque::new(),
            inbound: VecDeque::new(),
            accepted: None,
            connections: HashMap::default(),
            conn_events: HashMap::default(),
        }
    }

    pub fn drive(&mut self, now: Instant, remote: SocketAddr) {
        if let Some(ref socket) = self.socket {
            loop {
                let mut buf = [0; 8192];
                if socket.recv_from(&mut buf).is_err() {
                    break;
                }
            }
        }

        while self.inbound.front().map_or(false, |x| x.0 <= now) {
            let (recv_time, ecn, packet) = self.inbound.pop_front().unwrap();
            if let Some((ch, event)) =
                self.endpoint
                    .handle(recv_time, remote, None, ecn, packet.as_slice().into())
            {
                match event {
                    DatagramEvent::NewConnection(conn) => {
                        self.connections.insert(ch, conn);
                        self.accepted = Some(ch);
                    }
                    DatagramEvent::ConnectionEvent(event) => {
                        self.conn_events
                            .entry(ch)
                            .or_insert_with(VecDeque::new)
                            .push_back(event);
                    }
                }
            }
        }

        while let Some(x) = self.poll_transmit() {
            self.outbound.extend(split_transmit(x));
        }

        let mut endpoint_events: Vec<(ConnectionHandle, EndpointEvent)> = vec![];
        for (ch, conn) in self.connections.iter_mut() {
            if self.timeout.map_or(false, |x| x <= now) {
                self.timeout = None;
                conn.handle_timeout(now);
            }

            for (_, mut events) in self.conn_events.drain() {
                for event in events.drain(..) {
                    conn.handle_event(event);
                }
            }

            while let Some(event) = conn.poll_endpoint_events() {
                endpoint_events.push((*ch, event));
            }

            while let Some(x) = conn.poll_transmit(now, MAX_DATAGRAMS) {
                self.outbound.extend(split_transmit(x));
            }
            self.timeout = conn.poll_timeout();
        }

        for (ch, event) in endpoint_events {
            if let Some(event) = self.handle_event(ch, event) {
                if let Some(conn) = self.connections.get_mut(&ch) {
                    conn.handle_event(event);
                }
            }
        }
    }

    pub fn next_wakeup(&self) -> Option<Instant> {
        let next_inbound = self.inbound.front().map(|x| x.0);
        min_opt(self.timeout, next_inbound)
    }

    fn is_idle(&self) -> bool {
        self.connections.values().all(|x| x.is_idle())
    }

    pub fn delay_outbound(&mut self) {
        assert!(self.delayed.is_empty());
        mem::swap(&mut self.delayed, &mut self.outbound);
    }

    pub fn finish_delay(&mut self) {
        self.outbound.extend(self.delayed.drain(..));
    }

    pub fn assert_accept(&mut self) -> ConnectionHandle {
        self.accepted.take().expect("server didn't connect")
    }
}

impl ::std::ops::Deref for TestEndpoint {
    type Target = Endpoint;
    fn deref(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl ::std::ops::DerefMut for TestEndpoint {
    fn deref_mut(&mut self) -> &mut Endpoint {
        &mut self.endpoint
    }
}

pub fn subscribe() -> tracing::subscriber::DefaultGuard {
    let sub = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(|| TestWriter)
        .finish();
    tracing::subscriber::set_default(sub)
}

struct TestWriter;

impl Write for TestWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        print!(
            "{}",
            str::from_utf8(buf).expect("tried to log invalid UTF-8")
        );
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

pub fn server_config(cert_and_key: Option<(Certificate, PrivateKey)>) -> ServerConfig {
    let (cert, key) = match cert_and_key {
        Some((cert, key)) => (cert, key),
        None => (
            Certificate::from_der(&CERTIFICATE.serialize_der().unwrap()).unwrap(),
            PrivateKey::from_der(&CERTIFICATE.serialize_private_key_der()).unwrap(),
        ),
    };

    let cert_chain = CertificateChain::from_certs(vec![cert]);
    ServerConfig::with_single_cert(cert_chain, key).unwrap()
}

pub fn client_config(certs: Option<Vec<Certificate>>) -> ClientConfig {
    let certs = match certs {
        Some(certs) => certs,
        None => vec![Certificate::from_der(&CERTIFICATE.serialize_der().unwrap()).unwrap()],
    };

    let mut config = ClientConfig::with_root_certificates(certs, None).unwrap();
    Arc::make_mut(&mut config.crypto).key_log = Arc::new(KeyLogFile::new());
    Arc::make_mut(&mut config.crypto).enable_early_data = true;
    config
}

pub fn min_opt<T: Ord>(x: Option<T>, y: Option<T>) -> Option<T> {
    match (x, y) {
        (Some(x), Some(y)) => Some(cmp::min(x, y)),
        (Some(x), _) => Some(x),
        (_, Some(y)) => Some(y),
        _ => None,
    }
}

/// The maximum of datagrams TestEndpoint will produce via `poll_transmit`
const MAX_DATAGRAMS: usize = 10;

fn split_transmit(transmit: Transmit) -> Vec<Transmit> {
    let segment_size = match transmit.segment_size {
        Some(segment_size) => segment_size,
        _ => return vec![transmit],
    };

    let mut offset = 0;
    let mut transmits = Vec::new();
    while offset < transmit.contents.len() {
        let end = (offset + segment_size).min(transmit.contents.len());

        let contents = transmit.contents[offset..end].to_vec();
        transmits.push(Transmit {
            destination: transmit.destination,
            ecn: transmit.ecn,
            contents,
            segment_size: None,
            src_ip: transmit.src_ip,
        });

        offset = end;
    }

    transmits
}

lazy_static! {
    pub static ref SERVER_PORTS: Mutex<RangeFrom<u16>> = Mutex::new(4433..);
    pub static ref CLIENT_PORTS: Mutex<RangeFrom<u16>> = Mutex::new(44433..);
    pub(crate) static ref CERTIFICATE: rcgen::Certificate =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
}
