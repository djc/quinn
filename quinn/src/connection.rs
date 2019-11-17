use std::collections::HashMap;
use std::fmt;
use std::mem;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use err_derive::Error;
use futures::channel::{mpsc, oneshot};
use futures::task::{Context, Waker};
use futures::{Future, FutureExt, Poll, StreamExt};
use proto::{ConnectionError, ConnectionHandle, ConnectionId, Dir, StreamId, TimerUpdate};
use tokio_timer::{delay, Delay};
use tracing::{info_span, trace};

use crate::broadcast::{self, Broadcast};
use crate::streams::{RecvStream, SendStream, WriteError};
use crate::{ConnectionEvent, EndpointEvent, VarInt};

/// In-progress connection attempt future
///
/// Be sure to spawn the `ConnectionDriver` when complete.
pub struct Connecting(Option<ConnectionDriver>);

impl Connecting {
    pub(crate) fn new(conn: ConnectionRef) -> Self {
        Self(Some(ConnectionDriver(conn)))
    }

    /// Convert into a 0-RTT or 0.5-RTT connection at the cost of weakened security. Be sure to
    /// spawn the `ConnectionDriver`.
    ///
    /// Opens up the connection for use before the handshake finishes, allowing the API user to
    /// send data with 0-RTT encryption if the necessary key material is available. This is useful
    /// for reducing start-up latency by beginning transmission of application data without waiting
    /// for the handshake's cryptographic security guarantees to be established.
    ///
    /// When the `ZeroRttAccepted` future completes, the connection has been fully established.
    ///
    /// # Security
    ///
    /// On outgoing connections, this enables transmission of 0-RTT data, which might be vulnerable
    /// to replay attacks, and should therefore never invoke non-idempotent operations.
    ///
    /// On incoming connections, this enables transmission of 0.5-RTT data, which might be
    /// intercepted by a man-in-the-middle. If this occurs, the handshake will not complete
    /// successfully.
    ///
    /// # Errors
    ///
    /// Outgoing connections are only 0-RTT-capable when a cryptographic session ticket cached from
    /// a previous connection to the same server is available, and includes a 0-RTT key. If no such
    /// ticket is found, `self` is returned unmodified.
    ///
    /// For incoming connections, a 0.5-RTT connection will always be successfully constructed.
    pub fn into_0rtt(mut self) -> Result<(NewConnection, ZeroRttAccepted), Self> {
        // This lock borrows `self` and would normally be dropped at the end of this scope, so we'll
        // have to release it explicitly before returning `self` by value.
        let mut conn = (self.0.as_mut().unwrap().0).lock().unwrap();
        if conn.inner.has_0rtt() || conn.inner.side().is_server() {
            let (send, recv) = oneshot::channel();
            if conn.connected {
                send.send(true).unwrap();
            } else {
                conn.on_connected = Some(send);
            }
            drop(conn);
            let ConnectionDriver(conn) = self.0.take().unwrap();
            Ok((NewConnection::new(conn), ZeroRttAccepted(recv)))
        } else {
            drop(conn);
            Err(self)
        }
    }
}

impl Future for Connecting {
    type Output = Result<NewConnection, ConnectionError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let connected = match self.0 {
            Some(ref mut driver) => {
                let r = driver.poll_unpin(cx)?;
                let driver = self.0.as_mut().unwrap().0.lock().unwrap(); // borrowck workaround
                match r {
                    Poll::Ready(()) => {
                        return Poll::Ready(Err(driver.error.as_ref().unwrap().clone()));
                    }
                    Poll::Pending => driver.connected,
                }
            }
            None => panic!("polled after yielding Ready"),
        };
        if connected {
            let ConnectionDriver(conn) = self.0.take().unwrap();
            Poll::Ready(Ok(NewConnection::new(conn)))
        } else {
            Poll::Pending
        }
    }
}

/// Future that completes when a connection is fully established
///
/// For clients, the resulting value indicates if 0-RTT was accepted. For servers, the resulting
/// value is meaningless.
pub struct ZeroRttAccepted(oneshot::Receiver<bool>);

impl Future for ZeroRttAccepted {
    type Output = bool;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        self.0.poll_unpin(cx).map(|x| x.unwrap_or(false))
    }
}

/// Components of a newly established connection
///
/// Ensure `driver` runs or the connection will not work.
#[derive(Debug)]
pub struct NewConnection {
    /// The future responsible for handling I/O on the connection
    pub driver: ConnectionDriver,
    /// Handle for interacting with the connection
    pub connection: Connection,
    /// Unidirectional streams initiated by the peer, in the order they were opened
    pub uni_streams: IncomingUniStreams,
    /// Bidirectional streams initiated by the peer, in the order they were opened
    pub bi_streams: IncomingBiStreams,
    /// Unordered, unreliable datagrams sent by the peer
    pub datagrams: Datagrams,
    /// Leave room for future extensions
    pub(crate) _non_exhaustive: (),
}

impl NewConnection {
    fn new(conn: ConnectionRef) -> Self {
        Self {
            driver: ConnectionDriver(conn.clone()),
            connection: Connection(conn.clone()),
            uni_streams: IncomingUniStreams(conn.clone()),
            bi_streams: IncomingBiStreams(conn.clone()),
            datagrams: Datagrams(conn),
            _non_exhaustive: (),
        }
    }
}

/// A future that drives protocol logic for a connection
///
/// This future handles the protocol logic for a single connection, routing events from the
/// `Connection` API object to the `Endpoint` task and the related stream-related interfaces.
/// It also keeps track of outstanding timeouts for the `Connection`.
///
/// If the connection encounters an error condition, this future will yield an error. It will
/// terminate (yielding `Ok(())`) if the connection was closed without error. Unlike other
/// connection-related futures, this waits for the draining period to complete to ensure that
/// packets still in flight from the peer are handled gracefully.
#[must_use = "connection drivers must be spawned for their connections to function"]
#[derive(Debug)]
pub struct ConnectionDriver(pub(crate) ConnectionRef);

impl Future for ConnectionDriver {
    type Output = Result<(), ConnectionError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let conn = &mut *self.0.lock().unwrap();

        let span = info_span!("drive", id = conn.handle.0);
        let _guard = span.enter();

        loop {
            let now = Instant::now();
            let mut keep_going = false;
            if let Err(e) = conn.process_conn_events(cx) {
                conn.terminate(e.clone());
                return Poll::Ready(Err(e));
            }
            conn.drive_transmit(now);
            keep_going |= conn.drive_timers(cx, now);
            keep_going |= conn.handle_timer_updates();
            conn.forward_endpoint_events();
            conn.forward_app_events();
            if !keep_going || conn.inner.is_drained() {
                break;
            }
        }

        if !conn.inner.is_drained() {
            conn.driver = Some(cx.waker().clone());
            return Poll::Pending;
        }
        match conn.error {
            Some(ConnectionError::LocallyClosed) => Poll::Ready(Ok(())),
            Some(ref e) => Poll::Ready(Err(e.clone())),
            None => unreachable!("drained connections always have an error"),
        }
    }
}

/// A QUIC connection.
///
/// If all references to a connection (including every clone of the `Connection` handle, streams of
/// incoming streams, and the various stream types) other than the `ConnectionDriver` have been
/// dropped, the the connection will be automatically closed with an `error_code` of 0 and an empty
/// `reason`. You can also close the connection explicitly by calling `Connection::close()`.
///
/// May be cloned to obtain another handle to the same connection.
#[derive(Clone, Debug)]
pub struct Connection(ConnectionRef);

impl Connection {
    /// Initiate a new outgoing unidirectional stream.
    ///
    /// Streams are cheap and instantaneous to open unless blocked by flow control. As a
    /// consequence, the peer won't be notified that a stream has been opened until the stream is
    /// actually used.
    pub fn open_uni(&self) -> OpenUni {
        OpenUni {
            conn: self.0.clone(),
            state: broadcast::State::default(),
        }
    }

    /// Initiate a new outgoing bidirectional stream.
    ///
    /// Streams are cheap and instantaneous to open unless blocked by flow control. As a
    /// consequence, the peer won't be notified that a stream has been opened until the stream is
    /// actually used.
    pub fn open_bi(&self) -> OpenBi {
        OpenBi {
            conn: self.0.clone(),
            state: broadcast::State::default(),
        }
    }

    /// Close the connection immediately.
    ///
    /// Pending operations will fail immediately with `ConnectionError::LocallyClosed`. Delivery of
    /// data on unfinished streams is not guaranteed, so the application must call this only when
    /// all important communications have been completed.
    ///
    /// `error_code` and `reason` are not interpreted, and are provided directly to the peer.
    ///
    /// `reason` will be truncated to fit in a single packet with overhead; to improve odds that it
    /// is preserved in full, it should be kept under 1KiB.
    pub fn close(&self, error_code: VarInt, reason: &[u8]) {
        let conn = &mut *self.0.lock().unwrap();
        conn.close(error_code, reason.into());
    }

    /// Transmit `data` as an unreliable, unordered application datagram
    ///
    /// Application datagrams are a low-level primitive. They may be lost or delivered out of order,
    /// and `data` must both fit inside a single QUIC packet and be smaller than the maximum
    /// dictated by the peer.
    ///
    /// Will not wait unless the link is congested. The first call on a connection after
    /// `send_datagram_ready` completes successfully is guaranteed not to wait.
    pub fn send_datagram(&self, data: Bytes) -> SendDatagram<'_> {
        SendDatagram {
            conn: &self.0,
            data,
            state: broadcast::State::default(),
        }
    }

    /// Wait until the next `send_datagram` won't need to wait
    ///
    /// Useful when you don't want to materialize a datagram until the last possible moment before
    /// sending. Has no impact unless the link is congested.
    pub fn send_datagram_ready(&self) -> SendDatagramReady<'_> {
        SendDatagramReady {
            conn: &self.0,
            state: broadcast::State::default(),
        }
    }

    /// Compute the maximum size of datagrams that may passed to `send_datagram`
    ///
    /// Returns `None` if datagrams are unsupported by the peer or disabled locally.
    ///
    /// This may change over the lifetime of a connection according to variation in the path MTU
    /// estimate. The peer can also enforce an arbitrarily small fixed limit, but if the peer's
    /// limit is large this is guaranteed to be a little over a kilobyte at minimum.
    ///
    /// Not necessarily the maximum size of received datagrams.
    pub fn max_datagram_size(&self) -> Option<usize> {
        self.0.lock().unwrap().inner.max_datagram_size()
    }

    /// The peer's UDP address.
    pub fn remote_address(&self) -> SocketAddr {
        self.0.lock().unwrap().inner.remote()
    }

    /// The `ConnectionId` defined for `conn` by the peer.
    pub fn remote_id(&self) -> ConnectionId {
        self.0.lock().unwrap().inner.rem_cid()
    }

    /// The negotiated application protocol
    pub fn protocol(&self) -> Option<Box<[u8]>> {
        self.0.lock().unwrap().inner.protocol().map(|x| x.into())
    }

    // Update traffic keys spontaneously for testing purposes.
    #[doc(hidden)]
    pub fn force_key_update(&self) {
        self.0.lock().unwrap().inner.initiate_key_update()
    }
}

/// A stream of unidirectional QUIC streams initiated by a remote peer.
///
/// Incoming streams are *always* opened in the same order that the peer created them, but data can
/// be delivered to open streams in any order. This allows meaning to be assigned to the sequence in
/// which streams are opened. For example, a file transfer protocol might designate the first stream
/// the client opens as a "control" stream, using all others for exchanging file data.
///
/// Processing streams in the order they're opened will produce head-of-line blocking. For best
/// performance, an application should be prepared to fully process later streams before any data is
/// received on earlier streams.
#[derive(Debug)]
pub struct IncomingUniStreams(ConnectionRef);

impl futures::Stream for IncomingUniStreams {
    type Item = Result<RecvStream, ConnectionError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let mut conn = self.0.lock().unwrap();
        if let Some(x) = conn.inner.accept(Dir::Uni) {
            conn.wake(); // To send additional stream ID credit
            mem::drop(conn); // Release the lock so clone can take it
            Poll::Ready(Some(Ok(RecvStream::new(self.0.clone(), x, false))))
        } else if let Some(ConnectionError::LocallyClosed) = conn.error {
            Poll::Ready(None)
        } else if let Some(ref e) = conn.error {
            Poll::Ready(Some(Err(e.clone())))
        } else {
            conn.incoming_uni_streams_reader = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// A stream of bidirectional QUIC streams initiated by a remote peer.
///
/// See `IncomingUniStreams` for information about incoming streams in general.
#[derive(Debug)]
pub struct IncomingBiStreams(ConnectionRef);

impl futures::Stream for IncomingBiStreams {
    type Item = Result<(SendStream, RecvStream), ConnectionError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let mut conn = self.0.lock().unwrap();
        if let Some(x) = conn.inner.accept(Dir::Bi) {
            conn.wake(); // To send additional stream ID credit
            mem::drop(conn); // Release the lock so clone can take it
            Poll::Ready(Some(Ok((
                SendStream::new(self.0.clone(), x, false),
                RecvStream::new(self.0.clone(), x, false),
            ))))
        } else if let Some(ConnectionError::LocallyClosed) = conn.error {
            Poll::Ready(None)
        } else if let Some(ref e) = conn.error {
            Poll::Ready(Some(Err(e.clone())))
        } else {
            conn.incoming_bi_streams_reader = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Stream of unordered, unreliable datagrams sent by the peer
#[derive(Debug)]
pub struct Datagrams(ConnectionRef);

impl futures::Stream for Datagrams {
    type Item = Result<Bytes, ConnectionError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let mut conn = self.0.lock().unwrap();
        if let Some(x) = conn.inner.recv_datagram() {
            Poll::Ready(Some(Ok(x)))
        } else if let Some(ConnectionError::LocallyClosed) = conn.error {
            Poll::Ready(None)
        } else if let Some(ref e) = conn.error {
            Poll::Ready(Some(Err(e.clone())))
        } else {
            conn.datagram_reader = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// A future that will resolve into an opened outgoing unidirectional stream
pub struct OpenUni {
    conn: ConnectionRef,
    state: broadcast::State,
}

impl Future for OpenUni {
    type Output = Result<SendStream, ConnectionError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut conn = this.conn.lock().unwrap();
        if let Some(ref e) = conn.error {
            return Poll::Ready(Err(e.clone()));
        }
        if let Some(id) = conn.inner.open(Dir::Uni) {
            let is_0rtt = conn.inner.side().is_client() && conn.inner.is_handshaking();
            drop(conn); // Release lock for clone
            return Poll::Ready(Ok(SendStream::new(this.conn.clone(), id, is_0rtt)));
        }
        conn.uni_opening.register(cx, &mut this.state);
        Poll::Pending
    }
}

/// A future that will resolve into an opened outgoing bidirectional stream
pub struct OpenBi {
    conn: ConnectionRef,
    state: broadcast::State,
}

impl Future for OpenBi {
    type Output = Result<(SendStream, RecvStream), ConnectionError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut conn = this.conn.lock().unwrap();
        if let Some(ref e) = conn.error {
            return Poll::Ready(Err(e.clone()));
        }
        if let Some(id) = conn.inner.open(Dir::Bi) {
            let is_0rtt = conn.inner.side().is_client() && conn.inner.is_handshaking();
            drop(conn); // Release lock for clone
            return Poll::Ready(Ok((
                SendStream::new(this.conn.clone(), id, is_0rtt),
                RecvStream::new(this.conn.clone(), id, is_0rtt),
            )));
        }
        conn.bi_opening.register(cx, &mut this.state);
        Poll::Pending
    }
}

pub struct SendDatagramReady<'a> {
    conn: &'a ConnectionRef,
    state: broadcast::State,
}

impl<'a> Future for SendDatagramReady<'a> {
    type Output = Result<(), SendDatagramError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut conn = this.conn.lock().unwrap();
        if let Some(ref e) = conn.error {
            return Poll::Ready(Err(SendDatagramError::ConnectionClosed(e.clone())));
        }
        match conn.inner.send_datagram() {
            Ok(_) => Poll::Ready(Ok(())),
            Err(e) => conn.handle_datagram_err(cx, &mut this.state, e),
        }
    }
}

pub struct SendDatagram<'a> {
    conn: &'a ConnectionRef,
    data: Bytes,
    state: broadcast::State,
}

impl<'a> Future for SendDatagram<'a> {
    type Output = Result<(), SendDatagramError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut conn = this.conn.lock().unwrap();
        if let Some(ref e) = conn.error {
            return Poll::Ready(Err(SendDatagramError::ConnectionClosed(e.clone())));
        }
        match conn.inner.send_datagram() {
            Ok(sender) => match sender.send(mem::replace(&mut this.data, Bytes::new())) {
                Ok(()) => Poll::Ready(Ok(())),
                Err(proto::DatagramTooLarge) => Poll::Ready(Err(SendDatagramError::TooLarge)),
            },
            Err(e) => conn.handle_datagram_err(cx, &mut this.state, e),
        }
    }
}

/// Errors that arise from sending a datagram
#[derive(Debug, Error, Clone)]
pub enum SendDatagramError {
    /// The connection was closed.
    #[error(display = "connection closed: {}", 0)]
    ConnectionClosed(ConnectionError),
    /// The datagram is larger than the connection can currently accommodate
    ///
    /// Indicates that the path MTU minus overhead or the limit advertised by the peer has been
    /// exceeded.
    #[error(display = "datagram too large")]
    TooLarge,
    /// The peer does not support receiving datagram frames
    #[error(display = "datagrams not supported by peer")]
    UnsupportedByPeer,
    /// Datagram support is disabled locally
    #[error(display = "datagram support disabled")]
    Disabled,
}

#[derive(Debug)]
pub struct ConnectionRef(Arc<Mutex<ConnectionInner>>);

impl ConnectionRef {
    pub(crate) fn new(
        handle: ConnectionHandle,
        conn: proto::Connection,
        endpoint_events: mpsc::UnboundedSender<(ConnectionHandle, EndpointEvent)>,
        conn_events: mpsc::UnboundedReceiver<ConnectionEvent>,
    ) -> Self {
        Self(Arc::new(Mutex::new(ConnectionInner {
            epoch: Instant::now(),
            inner: conn,
            driver: None,
            handle,
            on_connected: None,
            connected: false,
            timers: Default::default(),
            conn_events,
            endpoint_events,
            blocked_writers: HashMap::new(),
            blocked_readers: HashMap::new(),
            uni_opening: Broadcast::new(),
            bi_opening: Broadcast::new(),
            incoming_uni_streams_reader: None,
            incoming_bi_streams_reader: None,
            datagram_reader: None,
            finishing: HashMap::new(),
            error: None,
            ref_count: 0,
            send_datagram_blocked: Broadcast::new(),
        })))
    }
}

impl Clone for ConnectionRef {
    fn clone(&self) -> Self {
        self.0.lock().unwrap().ref_count += 1;
        Self(self.0.clone())
    }
}

impl Drop for ConnectionRef {
    fn drop(&mut self) {
        let conn = &mut *self.0.lock().unwrap();
        if let Some(x) = conn.ref_count.checked_sub(1) {
            conn.ref_count = x;
            if x == 0 && !conn.inner.is_closed() {
                // If the driver is alive, it's just it and us, so we'd better shut it down. If it's
                // not, we can't do any harm. If there were any streams being opened, then either
                // the connection will be closed for an unrelated reason or a fresh reference will
                // be constructed for the newly opened stream.
                conn.implicit_close();
            }
        }
    }
}

impl std::ops::Deref for ConnectionRef {
    type Target = Mutex<ConnectionInner>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub struct ConnectionInner {
    epoch: Instant,
    pub(crate) inner: proto::Connection,
    driver: Option<Waker>,
    handle: ConnectionHandle,
    on_connected: Option<oneshot::Sender<bool>>,
    connected: bool,
    timers: proto::TimerTable<Option<Delay>>,
    conn_events: mpsc::UnboundedReceiver<ConnectionEvent>,
    endpoint_events: mpsc::UnboundedSender<(ConnectionHandle, EndpointEvent)>,
    pub(crate) blocked_writers: HashMap<StreamId, Waker>,
    pub(crate) blocked_readers: HashMap<StreamId, Waker>,
    uni_opening: Broadcast,
    bi_opening: Broadcast,
    incoming_uni_streams_reader: Option<Waker>,
    incoming_bi_streams_reader: Option<Waker>,
    datagram_reader: Option<Waker>,
    pub(crate) finishing: HashMap<StreamId, oneshot::Sender<Option<WriteError>>>,
    /// Always set to Some before the connection becomes drained
    pub(crate) error: Option<ConnectionError>,
    /// Number of live handles that can be used to initiate or handle I/O; excludes the driver
    ref_count: usize,
    send_datagram_blocked: Broadcast,
}

impl ConnectionInner {
    fn drive_transmit(&mut self, now: Instant) {
        while let Some(t) = self.inner.poll_transmit(now) {
            // If the endpoint driver is gone, noop.
            let _ = self
                .endpoint_events
                .unbounded_send((self.handle, EndpointEvent::Transmit(t)));
        }
    }

    fn forward_endpoint_events(&mut self) {
        while let Some(event) = self.inner.poll_endpoint_events() {
            // If the endpoint driver is gone, noop.
            let _ = self
                .endpoint_events
                .unbounded_send((self.handle, EndpointEvent::Proto(event)));
        }
    }

    /// If this returns `Err`, the endpoint is dead, so the driver should exit immediately.
    fn process_conn_events(&mut self, cx: &mut Context) -> Result<(), ConnectionError> {
        loop {
            match self.conn_events.poll_next_unpin(cx) {
                Poll::Ready(Some(ConnectionEvent::Proto(event))) => {
                    self.inner.handle_event(event);
                }
                Poll::Ready(Some(ConnectionEvent::Close { reason, error_code })) => {
                    self.close(error_code, reason);
                }
                Poll::Ready(None) => {
                    return Err(ConnectionError::TransportError(proto::TransportError {
                        code: proto::TransportErrorCode::INTERNAL_ERROR,
                        frame: None,
                        reason: "endpoint driver future was dropped".to_string(),
                    }));
                }
                Poll::Pending => {
                    return Ok(());
                }
            }
        }
    }

    fn forward_app_events(&mut self) {
        while let Some(event) = self.inner.poll() {
            use proto::Event::*;
            match event {
                Connected { .. } => {
                    self.connected = true;
                    if let Some(x) = self.on_connected.take() {
                        // We don't care if the on-connected future was dropped
                        let _ = x.send(self.inner.accepted_0rtt());
                    }
                }
                ConnectionLost { reason } => {
                    self.terminate(reason);
                }
                StreamWritable { stream } => {
                    if let Some(writer) = self.blocked_writers.remove(&stream) {
                        writer.wake();
                    }
                }
                StreamOpened { dir: Dir::Uni } => {
                    if let Some(x) = self.incoming_uni_streams_reader.take() {
                        x.wake();
                    }
                }
                StreamOpened { dir: Dir::Bi } => {
                    if let Some(x) = self.incoming_bi_streams_reader.take() {
                        x.wake();
                    }
                }
                DatagramReceived => {
                    if let Some(x) = self.datagram_reader.take() {
                        x.wake();
                    }
                }
                StreamReadable { stream } => {
                    if let Some(reader) = self.blocked_readers.remove(&stream) {
                        reader.wake();
                    }
                }
                StreamAvailable { dir } => {
                    let tasks = match dir {
                        Dir::Uni => &mut self.uni_opening,
                        Dir::Bi => &mut self.bi_opening,
                    };
                    tasks.wake();
                }
                StreamFinished {
                    stream,
                    stop_reason,
                } => {
                    if let Some(finishing) = self.finishing.remove(&stream) {
                        // If the finishing stream was already dropped, there's nothing more to do.
                        let _ = finishing
                            .send(stop_reason.map(|e| WriteError::Stopped { error_code: e }));
                    }
                }
                DatagramSendUnblocked => {
                    self.send_datagram_blocked.wake();
                }
            }
        }
    }

    fn drive_timers(&mut self, cx: &mut Context, now: Instant) -> bool {
        let mut keep_going = false;
        for (timer, slot) in &mut self.timers {
            if let Some(ref mut delay) = slot {
                match delay.poll_unpin(cx) {
                    Poll::Ready(()) => {
                        *slot = None;
                        trace!("{:?} timeout", timer);
                        self.inner.handle_timeout(now, timer);
                        // Timeout call may have queued sends
                        keep_going = true;
                    }
                    Poll::Pending => {}
                }
            }
        }
        keep_going
    }

    fn handle_timer_updates(&mut self) -> bool {
        let mut keep_going = false;
        while let Some(update) = self.inner.poll_timers() {
            keep_going = true; // Timers must be polled once set
            match update {
                TimerUpdate {
                    timer,
                    update: proto::TimerSetting::Start(time),
                } => match self.timers[timer] {
                    ref mut x @ None => {
                        trace!(time = ?time.duration_since(self.epoch), "{:?} timer start", timer);
                        *x = Some(delay(time));
                    }
                    Some(ref mut x) => {
                        trace!(time = ?time.duration_since(self.epoch), "{:?} timer reset", timer);
                        x.reset(time);
                    }
                },
                TimerUpdate {
                    timer,
                    update: proto::TimerSetting::Stop,
                } => {
                    if self.timers[timer].take().is_some() {
                        trace!("{:?} timer stop", timer);
                    }
                }
            }
        }
        keep_going
    }

    /// Wake up a blocked `Driver` task to process I/O
    pub(crate) fn wake(&mut self) {
        if let Some(x) = self.driver.take() {
            x.wake();
        }
    }

    /// Used to wake up all blocked futures when the connection becomes closed for any reason
    fn terminate(&mut self, reason: ConnectionError) {
        self.error = Some(reason.clone());
        for (_, writer) in self.blocked_writers.drain() {
            writer.wake()
        }
        for (_, reader) in self.blocked_readers.drain() {
            reader.wake()
        }
        self.uni_opening.wake();
        self.bi_opening.wake();
        if let Some(x) = self.incoming_uni_streams_reader.take() {
            x.wake();
        }
        if let Some(x) = self.incoming_bi_streams_reader.take() {
            x.wake();
        }
        if let Some(x) = self.datagram_reader.take() {
            x.wake();
        }
        for (_, x) in self.finishing.drain() {
            let _ = x.send(Some(WriteError::ConnectionClosed(reason.clone())));
        }
        self.send_datagram_blocked.wake();
        if let Some(x) = self.on_connected.take() {
            let _ = x.send(false);
        }
    }

    fn close(&mut self, error_code: VarInt, reason: Bytes) {
        self.inner.close(Instant::now(), error_code, reason);
        self.terminate(ConnectionError::LocallyClosed);
        self.wake();
    }

    /// Close for a reason other than the application's explicit request
    pub fn implicit_close(&mut self) {
        self.close(0u32.into(), Bytes::new());
    }

    pub(crate) fn check_0rtt(&self) -> Result<(), ()> {
        if self.inner.is_handshaking() || self.inner.accepted_0rtt() {
            Ok(())
        } else {
            Err(())
        }
    }

    fn handle_datagram_err(
        &mut self,
        cx: &mut Context,
        state: &mut broadcast::State,
        e: proto::SendDatagramError,
    ) -> Poll<Result<(), SendDatagramError>> {
        match e {
            proto::SendDatagramError::Blocked => {
                self.send_datagram_blocked.register(cx, state);
                Poll::Pending
            }
            proto::SendDatagramError::UnsupportedByPeer => {
                Poll::Ready(Err(SendDatagramError::UnsupportedByPeer))
            }
            proto::SendDatagramError::Disabled => Poll::Ready(Err(SendDatagramError::Disabled)),
        }
    }
}

impl Drop for ConnectionInner {
    fn drop(&mut self) {
        if !self.inner.is_drained() {
            // Ensure the endpoint can tidy up
            let _ = self.endpoint_events.unbounded_send((
                self.handle,
                EndpointEvent::Proto(proto::EndpointEvent::drained()),
            ));
        }
    }
}

impl fmt::Debug for ConnectionInner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ConnectionInner")
            .field("inner", &self.inner)
            .finish()
    }
}
