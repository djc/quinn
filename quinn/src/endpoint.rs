use std::{
    collections::VecDeque,
    future::Future,
    io,
    io::IoSliceMut,
    mem::MaybeUninit,
    net::{SocketAddr, SocketAddrV6},
    pin::Pin,
    str,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Instant,
};

use bytes::Bytes;
use futures::{channel::mpsc, StreamExt};
use fxhash::FxHashMap;
use proto::{self as proto, generic::ClientConfig, ConnectError, ConnectionHandle, DatagramEvent};

use crate::{
    broadcast::{self, Broadcast},
    builders::EndpointBuilder,
    connection::Connecting,
    platform::{RecvMeta, UdpSocket, BATCH_SIZE},
    work_limiter::WorkLimiter,
    ConnectionEvent, EndpointEvent, VarInt, IO_LOOP_BOUND, RECV_TIME_BOUND,
};

/// A QUIC endpoint.
///
/// An endpoint corresponds to a single UDP socket, may host many connections, and may act as both
/// client and server for different connections.
///
/// May be cloned to obtain another handle to the same endpoint.
#[derive(Debug)]
pub struct Endpoint<S>
where
    S: proto::crypto::Session,
{
    pub(crate) inner: EndpointRef<S>,
    pub(crate) default_client_config: Option<ClientConfig<S>>,
}

impl<S> Endpoint<S>
where
    S: proto::crypto::Session + 'static,
{
    /// Begin constructing an `Endpoint`
    pub fn builder() -> EndpointBuilder<S> {
        EndpointBuilder::default()
    }

    /// Connect to a remote endpoint
    ///
    /// `server_name` must be covered by the certificate presented by the server. This prevents a
    /// connection from being intercepted by an attacker with a valid certificate for some other
    /// server.
    ///
    /// May fail immediately due to configuration errors, or in the future if the connection could
    /// not be established.
    pub fn connect(
        &self,
        addr: &SocketAddr,
        server_name: &str,
    ) -> Result<Connecting<S>, ConnectError> {
        let config = match &self.default_client_config {
            Some(config) => config.clone(),
            None => return Err(ConnectError::NoDefaultClientConfig),
        };

        self.connect_with(config, addr, server_name)
    }

    /// Connect to a remote endpoint using a custom configuration.
    ///
    /// See [`connect()`] for details.
    ///
    /// [`connect()`]: Endpoint::connect
    pub fn connect_with(
        &self,
        config: ClientConfig<S>,
        addr: &SocketAddr,
        server_name: &str,
    ) -> Result<Connecting<S>, ConnectError> {
        let mut endpoint = self.inner.lock().unwrap();
        if endpoint.driver_lost {
            return Err(ConnectError::EndpointStopping);
        }
        if addr.is_ipv6() && !endpoint.ipv6 {
            return Err(ConnectError::InvalidRemoteAddress(*addr));
        }
        let addr = if endpoint.ipv6 {
            SocketAddr::V6(ensure_ipv6(*addr))
        } else {
            *addr
        };
        let (ch, conn) = endpoint.inner.connect(config, addr, server_name)?;
        Ok(endpoint.connections.insert(ch, conn))
    }

    /// Switch to a new UDP socket
    ///
    /// Allows the endpoint's address to be updated live, affecting all active connections. Incoming
    /// connections and connections to servers unreachable from the new address will be lost.
    ///
    /// On error, the old UDP socket is retained.
    pub fn rebind(&self, socket: std::net::UdpSocket) -> io::Result<()> {
        let addr = socket.local_addr()?;
        let socket = UdpSocket::from_std(socket)?;
        let mut inner = self.inner.lock().unwrap();
        inner.socket = socket;
        inner.ipv6 = addr.is_ipv6();
        Ok(())
    }

    /// Get the local `SocketAddr` the underlying socket is bound to
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.lock().unwrap().socket.local_addr()
    }

    /// Close all of this endpoint's connections immediately and cease accepting new connections.
    ///
    /// See [`Connection::close()`] for details.
    ///
    /// [`Connection::close()`]: crate::generic::Connection::close
    pub fn close(&self, error_code: VarInt, reason: &[u8]) {
        let reason = Bytes::copy_from_slice(reason);
        let mut endpoint = self.inner.lock().unwrap();
        endpoint.connections.close = Some((error_code, reason.clone()));
        for sender in endpoint.connections.senders.values() {
            // Ignoring errors from dropped connections
            let _ = sender.unbounded_send(ConnectionEvent::Close {
                error_code,
                reason: reason.clone(),
            });
        }
        if let Some(task) = endpoint.incoming_reader.take() {
            task.wake();
        }
    }

    /// Wait for all connections on the endpoint to be cleanly shut down
    ///
    /// Waiting for this condition before exiting ensures that a good-faith effort is made to notify
    /// peers of recent connection closes, whereas exiting immediately could force them to wait out
    /// the idle timeout period.
    ///
    /// Does not proactively close existing connections or cause incoming connections to be
    /// rejected. Consider calling [`close()`] and dropping the [`Incoming`] stream if
    /// that is desired.
    ///
    /// [`close()`]: Endpoint::close
    /// [`Incoming`]: crate::generic::Incoming
    pub async fn wait_idle(&self) {
        let mut state = broadcast::State::default();
        futures::future::poll_fn(|cx| {
            let endpoint = &mut *self.inner.lock().unwrap();
            if endpoint.connections.is_empty() {
                return Poll::Ready(());
            }
            endpoint.idle.register(cx, &mut state);
            Poll::Pending
        })
        .await;
    }
}

impl<S> Clone for Endpoint<S>
where
    S: proto::crypto::Session,
{
    fn clone(&self) -> Self {
        Endpoint {
            inner: self.inner.clone(),
            default_client_config: self.default_client_config.clone(),
        }
    }
}

/// A future that drives IO on an endpoint
///
/// This task functions as the switch point between the UDP socket object and the
/// `Endpoint` responsible for routing datagrams to their owning `Connection`.
/// In order to do so, it also facilitates the exchange of different types of events
/// flowing between the `Endpoint` and the tasks managing `Connection`s. As such,
/// running this task is necessary to keep the endpoint's connections running.
///
/// `EndpointDriver` futures terminate when the `Incoming` stream and all clones of the `Endpoint`
/// have been dropped, or when an I/O error occurs.
#[must_use = "endpoint drivers must be spawned for I/O to occur"]
#[derive(Debug)]
pub(crate) struct EndpointDriver<S: proto::crypto::Session>(pub(crate) EndpointRef<S>);

impl<S> Future for EndpointDriver<S>
where
    S: proto::crypto::Session + 'static,
{
    type Output = Result<(), io::Error>;

    #[allow(unused_mut)] // MSRV
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut endpoint = self.0.lock().unwrap();
        if endpoint.driver.is_none() {
            endpoint.driver = Some(cx.waker().clone());
        }

        let now = Instant::now();
        let mut keep_going = false;
        keep_going |= endpoint.drive_recv(cx, now)?;
        keep_going |= endpoint.handle_events(cx);
        keep_going |= endpoint.drive_send(cx)?;

        if !endpoint.incoming.is_empty() {
            if let Some(task) = endpoint.incoming_reader.take() {
                task.wake();
            }
        }

        if endpoint.ref_count == 0 && endpoint.connections.is_empty() {
            Poll::Ready(Ok(()))
        } else {
            drop(endpoint);
            // If there is more work to do schedule the endpoint task again.
            // `wake_by_ref()` is called outside the lock to minimize
            // lock contention on a multithreaded runtime.
            if keep_going {
                cx.waker().wake_by_ref();
            }
            Poll::Pending
        }
    }
}

impl<S> Drop for EndpointDriver<S>
where
    S: proto::crypto::Session,
{
    fn drop(&mut self) {
        let mut endpoint = self.0.lock().unwrap();
        endpoint.driver_lost = true;
        if let Some(task) = endpoint.incoming_reader.take() {
            task.wake();
        }
        // Drop all outgoing channels, signaling the termination of the endpoint to the associated
        // connections.
        endpoint.connections.senders.clear();
    }
}

#[derive(Debug)]
pub(crate) struct EndpointInner<S>
where
    S: proto::crypto::Session,
{
    socket: UdpSocket,
    inner: proto::generic::Endpoint<S>,
    outgoing: VecDeque<proto::Transmit>,
    incoming: VecDeque<Connecting<S>>,
    incoming_reader: Option<Waker>,
    driver: Option<Waker>,
    ipv6: bool,
    connections: ConnectionSet,
    events: mpsc::UnboundedReceiver<(ConnectionHandle, EndpointEvent)>,
    /// Number of live handles that can be used to initiate or handle I/O; excludes the driver
    ref_count: usize,
    driver_lost: bool,
    recv_limiter: WorkLimiter,
    recv_buf: Box<[u8]>,
    idle: Broadcast,
}

impl<S> EndpointInner<S>
where
    S: proto::crypto::Session + 'static,
{
    fn drive_recv<'a>(&'a mut self, cx: &mut Context, now: Instant) -> Result<bool, io::Error> {
        self.recv_limiter.start_cycle();
        let mut metas = [RecvMeta::default(); BATCH_SIZE];
        let mut iovs = MaybeUninit::<[IoSliceMut<'a>; BATCH_SIZE]>::uninit();
        self.recv_buf
            .chunks_mut(self.recv_buf.len() / BATCH_SIZE)
            .enumerate()
            .for_each(|(i, buf)| unsafe {
                iovs.as_mut_ptr()
                    .cast::<IoSliceMut>()
                    .add(i)
                    .write(IoSliceMut::<'a>::new(buf));
            });
        let mut iovs = unsafe { iovs.assume_init() };
        loop {
            match self.socket.poll_recv(cx, &mut iovs, &mut metas) {
                Poll::Ready(Ok(msgs)) => {
                    self.recv_limiter.record_work(msgs);
                    for (meta, buf) in metas.iter().zip(iovs.iter()).take(msgs) {
                        let data = buf[0..meta.len].into();
                        match self
                            .inner
                            .handle(now, meta.addr, meta.dst_ip, meta.ecn, data)
                        {
                            Some((handle, DatagramEvent::NewConnection(conn))) => {
                                let conn = self.connections.insert(handle, conn);
                                self.incoming.push_back(conn);
                            }
                            Some((handle, DatagramEvent::ConnectionEvent(event))) => {
                                // Ignoring errors from dropped connections that haven't yet been cleaned up
                                let _ = self
                                    .connections
                                    .senders
                                    .get_mut(&handle)
                                    .unwrap()
                                    .unbounded_send(ConnectionEvent::Proto(event));
                            }
                            None => {}
                        }
                    }
                }
                Poll::Pending => {
                    break;
                }
                // Ignore ECONNRESET as it's undefined in QUIC and may be injected by an
                // attacker
                Poll::Ready(Err(ref e)) if e.kind() == io::ErrorKind::ConnectionReset => {
                    continue;
                }
                Poll::Ready(Err(e)) => {
                    return Err(e);
                }
            }
            if !self.recv_limiter.allow_work() {
                self.recv_limiter.finish_cycle();
                return Ok(true);
            }
        }

        self.recv_limiter.finish_cycle();
        Ok(false)
    }

    fn drive_send(&mut self, cx: &mut Context) -> Result<bool, io::Error> {
        let mut transmits = 0;
        loop {
            while self.outgoing.len() < BATCH_SIZE {
                match self.inner.poll_transmit() {
                    Some(x) => self.outgoing.push_back(x),
                    None => break,
                }
            }
            if self.outgoing.is_empty() {
                return Ok(false);
            }
            match self.socket.poll_send(cx, self.outgoing.as_slices().0) {
                Poll::Ready(Ok(n)) => {
                    self.outgoing.drain(..n);
                    // We count transmits instead of `poll_send` calls since the cost
                    // of a `sendmmsg` still linearily increases with number of packets.
                    transmits += n;
                    if transmits >= IO_LOOP_BOUND {
                        return Ok(true);
                    }
                }
                Poll::Pending => {
                    return Ok(false);
                }
                Poll::Ready(Err(e)) => {
                    return Err(e);
                }
            }
        }
    }

    fn handle_events(&mut self, cx: &mut Context) -> bool {
        use EndpointEvent::*;

        for _ in 0..IO_LOOP_BOUND {
            match self.events.poll_next_unpin(cx) {
                Poll::Ready(Some((ch, event))) => match event {
                    Proto(e) => {
                        if e.is_drained() {
                            self.connections.senders.remove(&ch);
                            if self.connections.is_empty() {
                                self.idle.wake();
                            }
                        }
                        if let Some(event) = self.inner.handle_event(ch, e) {
                            // Ignoring errors from dropped connections that haven't yet been cleaned up
                            let _ = self
                                .connections
                                .senders
                                .get_mut(&ch)
                                .unwrap()
                                .unbounded_send(ConnectionEvent::Proto(event));
                        }
                    }
                    Transmit(t) => self.outgoing.push_back(t),
                },
                Poll::Ready(None) => unreachable!("EndpointInner owns one sender"),
                Poll::Pending => {
                    return false;
                }
            }
        }

        true
    }
}

#[derive(Debug)]
struct ConnectionSet {
    /// Senders for communicating with the endpoint's connections
    senders: FxHashMap<ConnectionHandle, mpsc::UnboundedSender<ConnectionEvent>>,
    /// Stored to give out clones to new ConnectionInners
    sender: mpsc::UnboundedSender<(ConnectionHandle, EndpointEvent)>,
    /// Set if the endpoint has been manually closed
    close: Option<(VarInt, Bytes)>,
}

impl ConnectionSet {
    fn insert<S: proto::crypto::Session + 'static>(
        &mut self,
        handle: ConnectionHandle,
        conn: proto::generic::Connection<S>,
    ) -> Connecting<S> {
        let (send, recv) = mpsc::unbounded();
        if let Some((error_code, ref reason)) = self.close {
            send.unbounded_send(ConnectionEvent::Close {
                error_code,
                reason: reason.clone(),
            })
            .unwrap();
        }
        self.senders.insert(handle, send);
        Connecting::new(handle, conn, self.sender.clone(), recv)
    }

    fn is_empty(&self) -> bool {
        self.senders.is_empty()
    }
}

fn ensure_ipv6(x: SocketAddr) -> SocketAddrV6 {
    match x {
        SocketAddr::V6(x) => x,
        SocketAddr::V4(x) => SocketAddrV6::new(x.ip().to_ipv6_mapped(), x.port(), 0, 0),
    }
}

/// Stream of incoming connections.
#[derive(Debug)]
pub struct Incoming<S: proto::crypto::Session>(EndpointRef<S>);

impl<S> Incoming<S>
where
    S: proto::crypto::Session,
{
    pub(crate) fn new(inner: EndpointRef<S>) -> Self {
        Self(inner)
    }
}

impl<S> futures::Stream for Incoming<S>
where
    S: proto::crypto::Session,
{
    type Item = Connecting<S>;

    #[allow(unused_mut)] // MSRV
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let endpoint = &mut *self.0.lock().unwrap();
        if endpoint.driver_lost {
            Poll::Ready(None)
        } else if let Some(conn) = endpoint.incoming.pop_front() {
            Poll::Ready(Some(conn))
        } else if endpoint.connections.close.is_some() {
            Poll::Ready(None)
        } else {
            endpoint.incoming_reader = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl<S> Drop for Incoming<S>
where
    S: proto::crypto::Session,
{
    fn drop(&mut self) {
        let endpoint = &mut *self.0.lock().unwrap();
        endpoint.inner.reject_new_connections();
        endpoint.incoming_reader = None;
    }
}

#[derive(Debug)]
pub(crate) struct EndpointRef<S: proto::crypto::Session>(Arc<Mutex<EndpointInner<S>>>);

impl<S> EndpointRef<S>
where
    S: proto::crypto::Session,
{
    pub(crate) fn new(socket: UdpSocket, inner: proto::generic::Endpoint<S>, ipv6: bool) -> Self {
        let recv_buf =
            vec![0; inner.config().get_max_udp_payload_size().min(64 * 1024) as usize * BATCH_SIZE];
        let (sender, events) = mpsc::unbounded();
        Self(Arc::new(Mutex::new(EndpointInner {
            socket,
            inner,
            ipv6,
            events,
            outgoing: VecDeque::new(),
            incoming: VecDeque::new(),
            incoming_reader: None,
            driver: None,
            connections: ConnectionSet {
                senders: FxHashMap::default(),
                sender,
                close: None,
            },
            ref_count: 0,
            driver_lost: false,
            recv_buf: recv_buf.into(),
            recv_limiter: WorkLimiter::new(RECV_TIME_BOUND),
            idle: Broadcast::new(),
        })))
    }
}

impl<S> Clone for EndpointRef<S>
where
    S: proto::crypto::Session,
{
    fn clone(&self) -> Self {
        self.0.lock().unwrap().ref_count += 1;
        Self(self.0.clone())
    }
}

impl<S> Drop for EndpointRef<S>
where
    S: proto::crypto::Session,
{
    fn drop(&mut self) {
        let endpoint = &mut *self.0.lock().unwrap();
        if let Some(x) = endpoint.ref_count.checked_sub(1) {
            endpoint.ref_count = x;
            if x == 0 {
                // If the driver is about to be on its own, ensure it can shut down if the last
                // connection is gone.
                if let Some(task) = endpoint.driver.take() {
                    task.wake();
                }
            }
        }
    }
}

impl<S> std::ops::Deref for EndpointRef<S>
where
    S: proto::crypto::Session,
{
    type Target = Mutex<EndpointInner<S>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
