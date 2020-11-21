use std::{
    collections::{HashMap, VecDeque},
    ffi::c_void,
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    pin::Pin,
    str,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Instant,
};

use bytes::Bytes;
use futures::channel::mpsc;
use proto::{self as proto, generic::ClientConfig, ConnectError, ConnectionHandle, DatagramEvent};
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket};
use winapi::{
    shared::{mswsockdef, ws2def, ws2ipdef},
    um::{memoryapi, mswsock, processthreadsapi, sysinfoapi, winbase, winnt, winsock2},
};

use crate::{
    broadcast::{self, Broadcast},
    builders::EndpointBuilder,
    connection::Connecting,
    platform::BATCH_SIZE,
    udp::{RecvMeta, UdpSocket},
    ConnectionEvent, EndpointEvent, VarInt, IO_LOOP_BOUND,
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
    pub(crate) default_client_config: ClientConfig<S>,
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
        self.connect_with(self.default_client_config.clone(), addr, server_name)
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
        let addr = if endpoint.ipv6 {
            SocketAddr::V6(ensure_ipv6(*addr))
        } else {
            *addr
        };
        let (ch, conn) = endpoint.inner.connect(config, addr, server_name)?;
        Ok(endpoint.connections.insert(ch, conn))
    }

    /// Get the local `SocketAddr` the underlying socket is bound to
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let guard = self.inner.lock().unwrap();
        let temp_socket: std::net::UdpSocket =
            unsafe { FromRawSocket::from_raw_socket(guard.socket as u64) };
        let result = temp_socket.local_addr();
        let _ = temp_socket.into_raw_socket();
        drop(guard);
        result
    }

    // /// Switch to a new UDP socket
    // ///
    // /// Allows the endpoint's address to be updated live, affecting all active connections. Incoming
    // /// connections and connections to servers unreachable from the new address will be lost.
    // ///
    // /// On error, the old UDP socket is retained.
    // pub fn rebind(&self, socket: std::net::UdpSocket) -> io::Result<()> {
    //     let addr = socket.local_addr()?;
    //     let socket = UdpSocket::from_std(socket)?;
    //     let mut inner = self.inner.lock().unwrap();
    //     inner.socket = socket;
    //     inner.ipv6 = addr.is_ipv6();
    //     Ok(())
    // }

    // /// Get the local `SocketAddr` the underlying socket is bound to
    // pub fn local_addr(&self) -> io::Result<SocketAddr> {
    //     self.inner.lock().unwrap().socket.local_addr()
    // }

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

impl<S> EndpointDriver<S>
where
    S: proto::crypto::Session + 'static,
{
    pub fn run(&mut self, ev: WsaEvent) -> Result<(), std::io::Error> {
        {
            let endpoint = &mut *self.0.lock().unwrap();
            unsafe {
                if endpoint.rio_handles.rio_table.RIONotify.as_ref().unwrap()(
                    endpoint.rio_handles.rio_cq,
                ) != 0
                {
                    println!("Rio Notify error: {:?}", wsa_last_error());
                }
            };
        }

        loop {
            ev.wait();

            let endpoint = &mut *self.0.lock().unwrap();
            ev.reset();

            let mut total_completions = 0;

            loop {
                let now = Instant::now();
                let mut keep_going = false;
                unsafe {
                    let (completions, kg) = endpoint.process_completions(now)?;
                    keep_going |= kg;
                    total_completions += completions;

                    endpoint.rio_buffers.ensure_recv_enqueued(
                        endpoint.rio_handles.rio_rq,
                        &endpoint.rio_handles.rio_table,
                    )?;

                    endpoint.handle_events();

                    endpoint.enqueue_transmits(now)?;
                }

                if !keep_going {
                    break;
                }
            }

            if !endpoint.incoming.is_empty() {
                if let Some(task) = endpoint.incoming_reader.take() {
                    task.wake();
                }
            }

            if endpoint.ref_count == 0 && endpoint.connections.is_empty() {
                return Ok(());
            }

            // Receive more RIO events
            if total_completions != 0 {
                unsafe {
                    let code = endpoint.rio_handles.rio_table.RIONotify.as_ref().unwrap()(
                        endpoint.rio_handles.rio_cq,
                    );
                    if code != 0 {
                        // eprintln!("Rio notify result: {} error: {:?}", code, wsa_last_error());
                    }
                };
            }
            drop(endpoint);
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

struct RioHandles {
    rio_table: mswsock::RIO_EXTENSION_FUNCTION_TABLE,
    rio_rq: mswsockdef::RIO_RQ,
    rio_cq: mswsockdef::RIO_CQ,
}

unsafe impl Send for RioHandles {}

pub(crate) struct EndpointInner<S>
where
    S: proto::crypto::Session,
{
    rio_handles: RioHandles,
    rio_buffers: RioBuffers,
    socket: winsock2::SOCKET,
    inner: proto::generic::Endpoint<S>,
    outgoing: VecDeque<proto::Transmit>,
    incoming: VecDeque<Connecting<S>>,
    incoming_reader: Option<Waker>,
    wakeup: WsaEvent,
    ipv6: bool,
    connections: ConnectionSet,
    events: MessageQueue<(ConnectionHandle, EndpointEvent)>,
    /// Number of live handles that can be used to initiate or handle I/O; excludes the driver
    ref_count: usize,
    driver_lost: bool,
    idle: Broadcast,
}

impl<S> std::fmt::Debug for EndpointInner<S>
where
    S: proto::crypto::Session,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointInner").finish()
    }
}

impl<S> EndpointInner<S>
where
    S: proto::crypto::Session + 'static,
{
    unsafe fn process_completions(&mut self, now: Instant) -> Result<(usize, bool), io::Error> {
        const MAX_RESULTS: usize = 64;

        let mut results = [mswsockdef::RIORESULT::default(); MAX_RESULTS];

        let num_completions = self
            .rio_handles
            .rio_table
            .RIODequeueCompletion
            .as_ref()
            .unwrap()(
            self.rio_handles.rio_cq,
            results.as_mut_ptr(),
            MAX_RESULTS as u32,
        );

        if mswsockdef::RIO_CORRUPT_CQ == num_completions {
            panic!("Corrupt CQ!");
        }

        let mut packets: HashMap<ConnectionHandle, VecDeque<proto::ConnectionEvent>> =
            HashMap::new();

        for result in results.iter().take(num_completions as usize) {
            let idx = result.RequestContext as usize;
            let is_transmit = idx >> 63 == 1;
            let idx = idx & 0x7F_FF_FF_FF_FF_FF_FF_FF;

            if is_transmit {
                self.process_transmit(now, result, idx)?;
            } else {
                self.process_receive(now, result, idx, &mut packets)?;
            }
        }

        for (handle, v) in packets {
            let _ = self
                .connections
                .senders
                .get_mut(&handle)
                .unwrap()
                .unbounded_send(ConnectionEvent::ProtoM(v));
        }

        Ok((
            num_completions as usize,
            num_completions as usize == MAX_RESULTS,
        ))
    }

    unsafe fn process_receive(
        &mut self,
        now: Instant,
        result: &mswsockdef::RIORESULT,
        idx: usize,
        packets: &mut HashMap<ConnectionHandle, VecDeque<proto::ConnectionEvent>>,
    ) -> Result<(), io::Error> {
        // println!("Received a packet");
        self.rio_buffers.unused_recv_buffers.push_back(idx);

        let buffer: &mut RioBuffer = &mut self.rio_buffers.recv_buffers[idx];
        buffer.inflight = false;
        let addr_buffer: &mut RioBuffer = &mut self.rio_buffers.recv_addr_buffers[idx];

        // println!("Received a packet with status: {}", result.Status);
        if result.Status != 0 {
            return Ok(());
        }

        let peer_addr: &ws2ipdef::SOCKADDR_INET =
            &*(addr_buffer.data as *const ws2ipdef::SOCKADDR_INET);
        let peer_rust_addr = into_sock_addr(peer_addr);

        let data: bytes::BytesMut =
            (&std::slice::from_raw_parts_mut(buffer.data, result.BytesTransferred as usize)[..])
                .into();

        // println!("Received {} bytes from {:?}", data.len(), peer_rust_addr);

        match self.inner.handle(now, peer_rust_addr, None, data) {
            Some((handle, DatagramEvent::NewConnection(conn))) => {
                let conn = self.connections.insert(handle, conn);
                self.incoming.push_back(conn);
            }
            Some((handle, DatagramEvent::ConnectionEvent(event))) => {
                // Ignoring errors from dropped connections that haven't yet been cleaned up
                packets.entry(handle).or_default().push_back(event);
            }
            None => {}
        }

        Ok(())
    }

    unsafe fn process_transmit(
        &mut self,
        _now: Instant,
        result: &mswsockdef::RIORESULT,
        idx: usize,
    ) -> Result<(), io::Error> {
        self.rio_buffers.unused_send_buffers.push_back(idx);
        let buffer: &mut RioBuffer = &mut self.rio_buffers.send_buffers[idx];
        buffer.inflight = false;
        // println!("Transmitted a packet of size {} with status: {}", buffer.rio_buf.Length, result.Status);
        if result.Status != 0 {}

        Ok(())
    }

    unsafe fn enqueue_transmits(&mut self, _now: Instant) -> Result<(), io::Error> {
        loop {
            //self.outgoing.len() < BATCH_SIZE {
            match self.inner.poll_transmit() {
                Some(x) => self.outgoing.push_back(x),
                None => break,
            }
        }

        while !self.rio_buffers.unused_send_buffers.is_empty() {
            let transmit = match self.outgoing.pop_front() {
                Some(transmit) => transmit,
                None => {
                    // println!("Nothing to send");
                    return Ok(());
                }
            };

            // println!("Enqueuing {} bytes for {:?}", transmit.contents.len(), transmit.destination);

            let buffer_idx = self.rio_buffers.unused_send_buffers.pop_front().unwrap();
            let mut buffer = &mut self.rio_buffers.send_buffers[buffer_idx];
            let mut addr_buffer = &mut self.rio_buffers.send_addr_buffers[buffer_idx];

            buffer.inflight = true;

            let len = core::cmp::min(transmit.contents.len(), SEND_BUFFER_SIZE);
            std::slice::from_raw_parts_mut(buffer.data, len)
                .copy_from_slice(&transmit.contents[..len]);
            buffer.rio_buf.Length = len as u32;
            if len < transmit.contents.len() {
                println!("Truncated buffer!");
            }

            let addr_data = addr_buffer.data as *mut ws2ipdef::SOCKADDR_INET;
            *addr_data = into_c_addr(transmit.destination);
            addr_buffer.rio_buf.Length = std::mem::size_of::<ws2ipdef::SOCKADDR_INET>() as u32;

            // Transmit operations are identified by setting the highest
            // bit to 1 for `RequestContext`
            let idx = buffer_idx | 0x80_00_00_00_00_00_00_00;

            let defer =
                !self.rio_buffers.unused_send_buffers.is_empty() && !self.outgoing.is_empty();

            if self.rio_handles.rio_table.RIOSendEx.as_ref().unwrap()(
                self.rio_handles.rio_rq,
                &mut buffer.rio_buf as *mut _,
                1,
                std::ptr::null_mut(),
                &mut addr_buffer.rio_buf as *mut _,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                if defer { mswsockdef::RIO_MSG_DEFER } else { 0 },
                idx as *mut libc::c_void,
            ) == 0
            {
                eprintln!("Send enqueue error");
                return Err(wsa_last_error());
            }
            // println!("Enqueued a packet of size {}", len);
        }

        println!("Out of socket buffers");

        Ok(())
    }

    fn handle_events(&mut self) {
        use EndpointEvent::*;
        loop {
            match self.events.try_pop() {
                Some((ch, event)) => match event {
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
                    Transmit(t) => {
                        self.outgoing.push_back(t);
                    }
                    TransmitM(mut transmits) => {
                        while let Some(transmit) = transmits.pop_front() {
                            self.outgoing.push_back(transmit);
                        }
                    }
                },
                None => {
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
struct ConnectionSet {
    /// Allows to spawn new connections
    spawner: tokio::runtime::Handle,
    /// Senders for communicating with the endpoint's connections
    senders: HashMap<ConnectionHandle, mpsc::UnboundedSender<ConnectionEvent>>,
    /// Stored to give out clones to new ConnectionInners
    sender: MessageSender<(ConnectionHandle, EndpointEvent)>,
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
        Connecting::new(&self.spawner, handle, conn, self.sender.clone(), recv)
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
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let endpoint = &mut *self.0.lock().unwrap();
        if endpoint.driver_lost {
            Poll::Ready(None)
        } else if let Some(conn) = endpoint.incoming.pop_front() {
            endpoint.inner.accept();
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

fn load_rio_function_table() -> mswsock::RIO_EXTENSION_FUNCTION_TABLE {
    let socket1 = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let raw_socket = socket1.as_raw_socket();

    let function_table_id = mswsock::WSAID_MULTIPLE_RIO;
    let mut table = mswsock::RIO_EXTENSION_FUNCTION_TABLE::default();

    let mut bytes_returned = 0u32;

    if unsafe {
        winsock2::WSAIoctl(
            raw_socket as _,
            ws2def::SIO_GET_MULTIPLE_EXTENSION_FUNCTION_POINTER,
            &function_table_id as *const _ as *mut std::ffi::c_void,
            std::mem::size_of_val(&function_table_id) as u32,
            &mut table as *mut _ as *mut _,
            std::mem::size_of_val(&table) as u32,
            &mut bytes_returned as *mut _,
            0 as *mut _,
            None,
        )
    } != 0
    {
        panic!("Can not load RIO extensions");
    }

    println!("Loaded RIO function table");
    table
}

const RIO_PENDING_RECVS: usize = 10000;
const RIO_PENDING_SENDS: usize = 10000;

const RECV_BUFFER_SIZE: usize = 2048;
const SEND_BUFFER_SIZE: usize = 2048;
const ADDR_BUFFER_SIZE: usize = 64;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum OperationType {
    None = 0,
    Recv = 1,
    Send = 2,
}

unsafe fn create_queues(
    event: &WsaEvent,
    socket_handle: winsock2::SOCKET,
    rio_table: &mswsock::RIO_EXTENSION_FUNCTION_TABLE,
) -> (mswsockdef::RIO_RQ, mswsockdef::RIO_CQ) {
    let mut completion_type = mswsock::RIO_NOTIFICATION_COMPLETION {
        Type: mswsock::RIO_EVENT_COMPLETION,
        u: Default::default(),
    };

    // NotifyReset is set to 0, because we also use the Event for other purposes
    // If calling the RIO function resets it, we might lose the notification
    // that e.g. something is stored in the endpoints input queue.
    *completion_type.u.Event_mut() = mswsock::RIO_NOTIFICATION_COMPLETION_u_s1 {
        EventHandle: event.inner.ev,
        NotifyReset: 0,
    };

    let completion_queue = rio_table.RIOCreateCompletionQueue.as_ref().unwrap()(
        RIO_PENDING_RECVS as u32 + RIO_PENDING_SENDS as u32,
        &mut completion_type as *mut _,
    );
    if completion_queue == mswsockdef::RIO_INVALID_CQ {
        panic!("can not create RIO completion queue");
    }

    let request_queue = rio_table.RIOCreateRequestQueue.as_ref().unwrap()(
        socket_handle,
        RIO_PENDING_RECVS as u32,
        1,
        RIO_PENDING_SENDS as u32,
        1,
        completion_queue,
        completion_queue,
        std::ptr::null_mut(),
    );
    if completion_queue == mswsockdef::RIO_INVALID_RQ {
        panic!("can not create RIO request queue");
    }

    println!("Setup request and completion queue");

    (request_queue, completion_queue)
}

fn round_down(value: usize, multiple: usize) -> usize {
    (value / multiple) * multiple
}

fn round_up(value: usize, multiple: usize) -> usize {
    round_down(value, multiple) + (if (value % multiple) > 0 { multiple } else { 0 })
}

unsafe fn allocate_buffer_space(
    buf_size: usize,
    buf_count: usize,
    total_buffer_size: &mut usize,
    total_buffer_count: &mut usize,
) -> *mut c_void {
    let mut system_info = sysinfoapi::SYSTEM_INFO::default();
    sysinfoapi::GetSystemInfo(&mut system_info);

    let granularity = system_info.dwAllocationGranularity as usize;
    let desired_size = buf_size * buf_count;

    let mut actual_size = round_up(desired_size, granularity);
    if actual_size > std::u32::MAX as usize {
        actual_size = (std::u32::MAX as usize / granularity) * granularity;
    }

    *total_buffer_count = core::cmp::min(buf_count, actual_size / buf_size);
    *total_buffer_size = actual_size;

    let buffer = memoryapi::VirtualAllocEx(
        processthreadsapi::GetCurrentProcess(),
        std::ptr::null_mut(),
        *total_buffer_size,
        winnt::MEM_COMMIT | winnt::MEM_RESERVE,
        winnt::PAGE_READWRITE,
    );

    if buffer.is_null() {
        panic!("Buffer allocation error");
    }

    return buffer;
}

struct RioBuffer {
    /// The buffer we are extending. This must be the first member of the struct
    rio_buf: mswsockdef::RIO_BUF,
    /// Whether the operation is in-flight
    inflight: bool,
    /// The type of the operation
    op_type: OperationType,
    /// The index of the buffer
    idx: usize,
    /// Start data pointer
    data: *mut u8,
}

struct RioBuffers {
    send_buffers: Vec<RioBuffer>,
    send_addr_buffers: Vec<RioBuffer>,
    recv_buffers: Vec<RioBuffer>,
    recv_addr_buffers: Vec<RioBuffer>,

    unused_send_buffers: VecDeque<usize>,
    unused_recv_buffers: VecDeque<usize>,
}

unsafe impl Send for RioBuffers {}

impl RioBuffers {
    pub unsafe fn new(rio_table: &mswsock::RIO_EXTENSION_FUNCTION_TABLE) -> Self {
        let send_buffers = allocate_rio_send_buffers(rio_table);
        let recv_buffers = allocate_rio_recv_buffers(rio_table);
        let send_addr_buffers = allocate_rio_send_addr_buffers(rio_table);
        let recv_addr_buffers = allocate_rio_recv_addr_buffers(rio_table);

        let mut unused_send_buffers = VecDeque::with_capacity(send_buffers.len());
        for i in 0..send_buffers.len() {
            unused_send_buffers.push_back(i);
        }

        let mut unused_recv_buffers = VecDeque::with_capacity(recv_buffers.len());
        for i in 0..recv_buffers.len() {
            unused_recv_buffers.push_back(i);
        }

        RioBuffers {
            send_buffers,
            send_addr_buffers,
            recv_buffers,
            recv_addr_buffers,
            unused_send_buffers,
            unused_recv_buffers,
        }
    }

    pub unsafe fn ensure_recv_enqueued(
        &mut self,
        request_queue: mswsockdef::RIO_RQ,
        rio_table: &mswsock::RIO_EXTENSION_FUNCTION_TABLE,
    ) -> Result<(), std::io::Error> {
        while let Some(send_idx) = self.unused_recv_buffers.pop_front() {
            let recv_buf = &mut self.recv_buffers[send_idx];
            recv_buf.inflight = true;
            let addr_buf = &mut self.recv_addr_buffers[send_idx];

            let defer = !self.unused_recv_buffers.is_empty();

            if rio_table.RIOReceiveEx.as_ref().unwrap()(
                request_queue,
                &mut recv_buf.rio_buf as *mut mswsockdef::RIO_BUF,
                1,
                std::ptr::null_mut(),
                &mut addr_buf.rio_buf as *mut mswsockdef::RIO_BUF,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                if defer { mswsockdef::RIO_MSG_DEFER } else { 0 },
                send_idx as *mut _,
            ) != 1
            {
                eprintln!("Receive enqueue error");
                return Err(wsa_last_error());
            };

            // println!("Enqueued a receive");
        }

        Ok(())
    }
}

unsafe fn allocate_rio_send_buffers(
    rio_table: &mswsock::RIO_EXTENSION_FUNCTION_TABLE,
) -> Vec<RioBuffer> {
    allocate_rio_buffer(
        rio_table,
        SEND_BUFFER_SIZE,
        RIO_PENDING_SENDS,
        OperationType::Send,
    )
}

unsafe fn allocate_rio_send_addr_buffers(
    rio_table: &mswsock::RIO_EXTENSION_FUNCTION_TABLE,
) -> Vec<RioBuffer> {
    allocate_rio_buffer(
        rio_table,
        ADDR_BUFFER_SIZE,
        RIO_PENDING_SENDS,
        OperationType::None,
    )
}

unsafe fn allocate_rio_recv_buffers(
    rio_table: &mswsock::RIO_EXTENSION_FUNCTION_TABLE,
) -> Vec<RioBuffer> {
    allocate_rio_buffer(
        rio_table,
        RECV_BUFFER_SIZE,
        RIO_PENDING_RECVS,
        OperationType::Recv,
    )
}

unsafe fn allocate_rio_recv_addr_buffers(
    rio_table: &mswsock::RIO_EXTENSION_FUNCTION_TABLE,
) -> Vec<RioBuffer> {
    allocate_rio_buffer(
        rio_table,
        ADDR_BUFFER_SIZE,
        RIO_PENDING_RECVS,
        OperationType::None,
    )
}

unsafe fn allocate_rio_buffer(
    rio_table: &mswsock::RIO_EXTENSION_FUNCTION_TABLE,
    buf_size: usize,
    buf_count: usize,
    op_type: OperationType,
) -> Vec<RioBuffer> {
    let mut total_buffer_count = 0;
    let mut total_buffer_size = 0;

    let buffer_ptr = allocate_buffer_space(
        buf_size,
        buf_count,
        &mut total_buffer_size,
        &mut total_buffer_count,
    );
    let buffer_id = rio_table.RIORegisterBuffer.as_ref().unwrap()(
        buffer_ptr as *mut _,
        total_buffer_size as u32,
    );

    if buffer_id == mswsockdef::RIO_INVALID_BUFFERID {
        panic!("Invalid send buffer ID");
    }

    let mut offset = 0usize;
    let mut send_bufs = Vec::with_capacity(total_buffer_count);
    for i in 0..total_buffer_count {
        let buf = RioBuffer {
            op_type,
            inflight: false,
            idx: i,
            data: buffer_ptr.offset(offset as isize) as *mut _,
            rio_buf: mswsockdef::RIO_BUF {
                BufferId: buffer_id,
                Offset: offset as _,
                Length: buf_size as u32,
            },
        };

        send_bufs.push(buf);
        offset += buf_size;
    }

    send_bufs
}

struct WsaEventInner {
    ev: *mut c_void,
}

impl WsaEventInner {
    pub fn new() -> Self {
        let event = unsafe { winsock2::WSACreateEvent() };
        Self { ev: event }
    }
}

impl Drop for WsaEventInner {
    fn drop(&mut self) {
        unsafe {
            winsock2::WSACloseEvent(self.ev);
        }
    }
}

#[derive(Clone)]
pub(crate) struct WsaEvent {
    inner: Arc<WsaEventInner>,
}

unsafe impl Send for WsaEvent {}
unsafe impl Sync for WsaEvent {}

impl WsaEvent {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(WsaEventInner::new()),
        }
    }

    pub fn wait(&self) {
        unsafe {
            winapi::um::synchapi::WaitForSingleObject(self.inner.ev, winsock2::WSA_INFINITE);
        }
    }

    pub fn reset(&self) {
        unsafe {
            winsock2::WSAResetEvent(self.inner.ev);
        }
    }

    pub fn signal(&self) {
        unsafe {
            winsock2::WSASetEvent(self.inner.ev);
        }
    }
}

pub(crate) struct MessageQueue<T> {
    inner: Arc<Mutex<VecDeque<T>>>,
}

impl<T> Clone for MessageQueue<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> MessageQueue<T> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn send(&self, item: T) {
        let mut guard = self.inner.lock().unwrap();
        guard.push_back(item);
    }

    pub fn try_pop(&self) -> Option<T> {
        let mut guard = self.inner.lock().unwrap();
        guard.pop_front()
    }
}

pub(crate) struct MessageSender<T> {
    queue: MessageQueue<T>,
    wakeup: WsaEvent,
}

impl<T> Clone for MessageSender<T> {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
            wakeup: self.wakeup.clone(),
        }
    }
}

impl<T> std::fmt::Debug for MessageSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageSender").finish()
    }
}

impl<T> MessageSender<T> {
    pub fn new(queue: MessageQueue<T>, wakeup: WsaEvent) -> Self {
        Self { queue, wakeup }
    }

    pub fn send(&self, item: T) {
        self.queue.send(item);
        self.wakeup.signal();
    }
}

#[derive(Debug)]
pub(crate) struct EndpointRef<S: proto::crypto::Session>(Arc<Mutex<EndpointInner<S>>>);

impl<S> EndpointRef<S>
where
    S: proto::crypto::Session,
{
    pub(crate) fn new(
        spawner: tokio::runtime::Handle,
        socket: std::net::UdpSocket,
        inner: proto::generic::Endpoint<S>,
        ipv6: bool,
    ) -> Self {
        let mut winsock_data = winsock2::WSADATA::default();
        if unsafe { winsock2::WSAStartup(0x202, &mut winsock_data) } != 0 {
            panic!("Error starting winsock");
        }

        let raw_socket = socket.into_raw_socket() as usize;

        let rio_table = load_rio_function_table();

        let event = WsaEvent::new();
        event.signal();
        let msg_queue = MessageQueue::new();
        let endpoint_event_sender = MessageSender::new(msg_queue.clone(), event.clone());

        let (rio_rq, rio_cq) = unsafe { create_queues(&event, raw_socket, &rio_table) };

        let rio_buffers = unsafe { RioBuffers::new(&rio_table) };

        Self(Arc::new(Mutex::new(EndpointInner {
            rio_buffers,
            rio_handles: RioHandles {
                rio_table,
                rio_rq,
                rio_cq,
            },
            socket: raw_socket,
            inner,
            ipv6,
            events: msg_queue,
            outgoing: VecDeque::new(),
            incoming: VecDeque::new(),
            incoming_reader: None,
            wakeup: event.clone(),
            connections: ConnectionSet {
                spawner,
                senders: HashMap::new(),
                sender: endpoint_event_sender,
                close: None,
            },
            ref_count: 0,
            driver_lost: false,
            idle: Broadcast::new(),
        })))
    }

    pub(crate) fn wakeup_event(&self) -> WsaEvent {
        let guard = self.0.lock().unwrap();
        guard.wakeup.clone()
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
                endpoint.wakeup.signal();
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

pub fn into_c_addr(sock_addr: SocketAddr) -> ws2ipdef::SOCKADDR_INET {
    let mut result = ws2ipdef::SOCKADDR_INET::default();

    unsafe {
        match sock_addr {
            SocketAddr::V4(v4) => {
                *result.si_family_mut() = ws2def::AF_INET as u16;
                result.Ipv4_mut().sin_family = ws2def::AF_INET as u16;
                result.Ipv4_mut().sin_port = v4.port().to_be();

                let mut dst = result.Ipv4_mut().sin_addr.S_un.S_un_b_mut();
                dst.s_b1 = v4.ip().octets()[0];
                dst.s_b2 = v4.ip().octets()[1];
                dst.s_b3 = v4.ip().octets()[2];
                dst.s_b4 = v4.ip().octets()[3];
            }
            SocketAddr::V6(v6) => {
                *result.si_family_mut() = ws2def::AF_INET6 as u16;
                result.Ipv6_mut().sin6_family = ws2def::AF_INET6 as u16;
                result.Ipv6_mut().sin6_port = v6.port().to_be();

                let dst = result.Ipv6_mut().sin6_addr.u.Word_mut();
                *dst = v6.ip().segments();
            }
        }
    }

    result
}

unsafe fn into_sock_addr(c_addr: &ws2ipdef::SOCKADDR_INET) -> SocketAddr {
    match *c_addr.si_family() as libc::c_int {
        ws2def::AF_INET => {
            let v4 = c_addr.Ipv4();
            // Note: This should likely be `to_le`- but it yields the wrong result?
            let port = v4.sin_port.to_be();
            let addr = v4.sin_addr.S_un.S_un_b();

            let ip = Ipv4Addr::new(addr.s_b1, addr.s_b2, addr.s_b3, addr.s_b4);
            SocketAddr::V4(SocketAddrV4::new(ip, port))
        }
        ws2def::AF_INET6 => {
            let v6 = c_addr.Ipv6();
            let port = v6.sin6_port.to_le();
            let addr = v6.sin6_addr.u.Word();

            let ip = Ipv6Addr::new(
                addr[0], addr[1], addr[2], addr[3], addr[4], addr[5], addr[6], addr[7],
            );
            SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0))
        }
        other => {
            panic!("Unknown address familiy {}", other);
        }
    }
}

pub(crate) fn wsa_last_error() -> std::io::Error {
    unsafe {
        let last_error = winsock2::WSAGetLastError();

        let mut buf = [0u8; 256];

        let len = winbase::FormatMessageA(
            winbase::FORMAT_MESSAGE_FROM_SYSTEM | winbase::FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            last_error as u32,
            winnt::MAKELANGID(winnt::LANG_NEUTRAL, winnt::SUBLANG_DEFAULT) as u32,
            buf.as_mut_ptr() as *mut i8,
            buf.len() as u32,
            std::ptr::null_mut(),
        );

        let error_str = std::str::from_utf8(&buf[..len as usize]).unwrap_or("Invalid string");

        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!(
                "WSAGetLastError: Code: {}. Message: {}",
                last_error, error_str
            ),
        )
    }
}
