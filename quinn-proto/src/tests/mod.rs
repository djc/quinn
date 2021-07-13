use std::{
    convert::TryInto,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use assert_matches::assert_matches;
use bytes::Bytes;
use hex_literal::hex;
use rand::RngCore;
use ring::hmac;
use rustls::internal::msgs::enums::AlertDescription;
use tracing::info;

use super::*;
use crate::cid_generator::{ConnectionIdGenerator, RandomConnectionIdGenerator};
use crate::crypto::Session as _;
use crate::{Certificate, CertificateChain, PrivateKey};
mod util;
use util::*;

#[test]
fn version_negotiate_server() {
    let _guard = subscribe();
    let client_addr = "[::2]:7890".parse().unwrap();
    let mut server = Endpoint::new(Default::default(), Some(Arc::new(server_config())));
    let now = Instant::now();
    let event = server.handle(
        now,
        client_addr,
        None,
        None,
        // Long-header packet with reserved version number
        hex!("80 0a1a2a3a 04 00000000 04 00000000 00")[..].into(),
    );
    assert!(event.is_none());

    let io = server.poll_transmit();
    assert!(io.is_some());
    if let Some(Transmit { contents, .. }) = io {
        assert_ne!(contents[0] & 0x80, 0);
        assert_eq!(&contents[1..15], hex!("00000000 04 00000000 04 00000000"));
        assert!(contents[15..].chunks(4).any(|x| {
            DEFAULT_SUPPORTED_VERSIONS.contains(&u32::from_be_bytes(x.try_into().unwrap()))
        }));
    }
    assert_matches!(server.poll_transmit(), None);
}

#[test]
fn version_negotiate_client() {
    let _guard = subscribe();
    let server_addr = "[::2]:7890".parse().unwrap();
    let cid_generator_factory: fn() -> Box<dyn ConnectionIdGenerator> =
        || Box::new(RandomConnectionIdGenerator::new(0));
    let mut client = Endpoint::new(
        Arc::new(EndpointConfig {
            connection_id_generator_factory: Arc::new(cid_generator_factory),
            ..Default::default()
        }),
        None,
    );
    let (_, mut client_ch) = client
        .connect(client_config(), server_addr, "localhost")
        .unwrap();
    let now = Instant::now();
    let opt_event = client.handle(
        now,
        server_addr,
        None,
        None,
        // Version negotiation packet for reserved version
        hex!(
            "80 00000000 04 00000000 04 00000000
             0a1a2a3a"
        )[..]
            .into(),
    );
    if let Some((_, DatagramEvent::ConnectionEvent(event))) = opt_event {
        client_ch.handle_event(event);
    }
    assert_matches!(
        client_ch.poll(),
        Some(Event::ConnectionLost {
            reason: ConnectionError::VersionMismatch,
        })
    );
}

#[test]
fn lifecycle() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);
    assert!(pair.client_conn_mut(client_ch).using_ecn());
    assert!(pair.server_conn_mut(server_ch).using_ecn());

    const REASON: &[u8] = b"whee";
    info!("closing");
    pair.client.connections.get_mut(&client_ch).unwrap().close(
        pair.time,
        VarInt(42),
        REASON.into(),
    );
    pair.drive();
    assert_matches!(pair.server_conn_mut(server_ch).poll(),
                    Some(Event::ConnectionLost { reason: ConnectionError::ApplicationClosed(
                        ApplicationClose { error_code: VarInt(42), ref reason }
                    )}) if reason == REASON);
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);
    assert_eq!(pair.client.known_connections(), 0);
    assert_eq!(pair.client.known_cids(), 0);
    assert_eq!(pair.server.known_connections(), 0);
    assert_eq!(pair.server.known_cids(), 0);
}

#[test]
fn stateless_retry() {
    let _guard = subscribe();
    let mut pair = Pair::new(
        Default::default(),
        ServerConfig {
            use_stateless_retry: true,
            ..server_config()
        },
    );
    pair.connect();
}

#[test]
fn server_stateless_reset() {
    let _guard = subscribe();
    let mut reset_key = vec![0; 64];
    let mut rng = rand::thread_rng();
    rng.fill_bytes(&mut reset_key);
    let reset_key = hmac::Key::new(hmac::HMAC_SHA256, &reset_key);

    let endpoint_config = Arc::new(EndpointConfig::new(reset_key));

    let mut pair = Pair::new(endpoint_config.clone(), server_config());
    let (client_ch, _) = pair.connect();
    pair.server.endpoint = Endpoint::new(endpoint_config, Some(Arc::new(server_config())));
    // Send something big enough to allow room for a smaller stateless reset.
    pair.client.connections.get_mut(&client_ch).unwrap().close(
        pair.time,
        VarInt(42),
        (&[0xab; 128][..]).into(),
    );
    info!("resetting");
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::ConnectionLost {
            reason: ConnectionError::Reset
        })
    );
}

#[test]
fn client_stateless_reset() {
    let _guard = subscribe();
    let mut reset_key = vec![0; 64];
    let mut rng = rand::thread_rng();
    rng.fill_bytes(&mut reset_key);
    let reset_key = hmac::Key::new(hmac::HMAC_SHA256, &reset_key);

    let endpoint_config = Arc::new(EndpointConfig::new(reset_key));

    let mut pair = Pair::new(endpoint_config.clone(), server_config());
    let (_, server_ch) = pair.connect();
    pair.client.endpoint = Endpoint::new(endpoint_config, Some(Arc::new(server_config())));
    // Send something big enough to allow room for a smaller stateless reset.
    pair.server.connections.get_mut(&server_ch).unwrap().close(
        pair.time,
        VarInt(42),
        (&[0xab; 128][..]).into(),
    );
    info!("resetting");
    pair.drive();
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::ConnectionLost {
            reason: ConnectionError::Reset
        })
    );
}

#[test]
fn export_keying_material() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    const LABEL: &[u8] = b"test_label";
    const CONTEXT: &[u8] = b"test_context";

    // client keying material
    let mut client_buf = [0u8; 64];
    pair.client_conn_mut(client_ch)
        .crypto_session()
        .export_keying_material(&mut client_buf, LABEL, CONTEXT)
        .unwrap();

    // server keying material
    let mut server_buf = [0u8; 64];
    pair.server_conn_mut(server_ch)
        .crypto_session()
        .export_keying_material(&mut server_buf, LABEL, CONTEXT)
        .unwrap();

    assert_eq!(&client_buf[..], &server_buf[..]);
}

#[test]
fn finish_stream_simple() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();

    const MSG: &[u8] = b"hello";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    assert_eq!(pair.client_streams(client_ch).send_streams(), 1);
    pair.client_send(client_ch, s).finish().unwrap();
    pair.drive();

    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Stream(StreamEvent::Finished { id })) if id == s
    );
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);
    assert_eq!(pair.client_streams(client_ch).send_streams(), 0);
    assert_eq!(pair.server_conn_mut(client_ch).streams().send_streams(), 0);
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Uni }))
    );
    // Receive-only streams do not get `StreamFinished` events
    assert_eq!(pair.server_conn_mut(client_ch).streams().send_streams(), 0);
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(
        chunks.next(usize::MAX),
        Ok(Some(chunk)) if chunk.offset == 0 && chunk.bytes == MSG
    );
    assert_matches!(chunks.next(usize::MAX), Ok(None));
    let _ = chunks.finalize();
}

#[test]
fn reset_stream() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();

    const MSG: &[u8] = b"hello";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    pair.drive();

    info!("resetting stream");
    const ERROR: VarInt = VarInt(42);
    pair.client_send(client_ch, s).reset(ERROR).unwrap();
    pair.drive();

    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Uni }))
    );
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);
    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(chunks.next(usize::MAX), Err(ReadError::Reset(ERROR)));
    let _ = chunks.finalize();
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);
}

#[test]
fn stop_stream() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    const MSG: &[u8] = b"hello";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    pair.drive();

    info!("stopping stream");
    const ERROR: VarInt = VarInt(42);
    pair.server_recv(server_ch, s).stop(ERROR).unwrap();
    pair.drive();

    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Uni }))
    );
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);

    assert_matches!(
        pair.client_send(client_ch, s).write(b"foo"),
        Err(WriteError::Stopped(ERROR))
    );
    assert_matches!(
        pair.client_send(client_ch, s).finish(),
        Err(FinishError::Stopped(ERROR))
    );
}

#[test]
fn reject_self_signed_server_cert() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    info!("connecting");
    let client_ch = pair.begin_connect(ClientConfig::default());
    pair.drive();
    assert_matches!(pair.client_conn_mut(client_ch).poll(),
                    Some(Event::ConnectionLost { reason: ConnectionError::TransportError(ref error)})
                    if error.code == TransportErrorCode::crypto(AlertDescription::BadCertificate.get_u8()));
}

#[test]
fn reject_missing_client_cert() {
    let _guard = subscribe();
    let mut server_config = server_config();
    Arc::make_mut(&mut server_config.crypto).set_client_certificate_verifier(
        rustls::AllowAnyAuthenticatedClient::new(rustls::RootCertStore::empty()),
    );
    let mut pair = Pair::new(Default::default(), server_config);
    info!("connecting");
    let client_ch = pair.begin_connect(client_config());
    pair.drive();

    // The client completes the connection, but finds it immediately closed
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Connected)
    );
    assert_matches!(pair.client_conn_mut(client_ch).poll(),
                    Some(Event::ConnectionLost { reason: ConnectionError::ConnectionClosed(ref close)})
                    if close.error_code == TransportErrorCode::crypto(AlertDescription::CertificateRequired.get_u8()));

    // The server never completes the connection
    let server_ch = pair.server.assert_accept();
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(pair.server_conn_mut(server_ch).poll(),
                    Some(Event::ConnectionLost { reason: ConnectionError::TransportError(ref error)})
                    if error.code == TransportErrorCode::crypto(AlertDescription::CertificateRequired.get_u8()));
}

#[test]
fn congestion() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, _) = pair.connect();

    const TARGET: u64 = 2048;
    assert!(pair.client_conn_mut(client_ch).congestion_window() > TARGET);
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    // Send data without receiving ACKs until the congestion state falls below target
    while pair.client_conn_mut(client_ch).congestion_window() > TARGET {
        let n = pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
        assert_eq!(n, 1024);
        pair.drive_client();
    }
    // Ensure that the congestion state recovers after receiving the ACKs
    pair.drive();
    assert!(pair.client_conn_mut(client_ch).congestion_window() >= TARGET);
    pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
}

#[allow(clippy::field_reassign_with_default)] // https://github.com/rust-lang/rust-clippy/issues/6527
#[test]
fn high_latency_handshake() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    pair.latency = Duration::from_micros(200 * 1000);
    let (client_ch, server_ch) = pair.connect();
    assert_eq!(pair.client_conn_mut(client_ch).bytes_in_flight(), 0);
    assert_eq!(pair.server_conn_mut(server_ch).bytes_in_flight(), 0);
    assert!(pair.client_conn_mut(client_ch).using_ecn());
    assert!(pair.server_conn_mut(server_ch).using_ecn());
}

#[test]
fn zero_rtt_happypath() {
    let _guard = subscribe();
    let mut pair = Pair::new(
        Default::default(),
        ServerConfig {
            use_stateless_retry: true,
            ..server_config()
        },
    );
    let config = client_config();

    // Establish normal connection
    let client_ch = pair.begin_connect(config.clone());
    pair.drive();
    pair.server.assert_accept();
    pair.client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .close(pair.time, VarInt(0), [][..].into());
    pair.drive();

    pair.client.addr = SocketAddr::new(
        Ipv6Addr::LOCALHOST.into(),
        CLIENT_PORTS.lock().unwrap().next().unwrap(),
    );
    info!("resuming session");
    let client_ch = pair.begin_connect(config);
    assert!(pair.client_conn_mut(client_ch).has_0rtt());
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    const MSG: &[u8] = b"Hello, 0-RTT!";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    pair.drive();
    assert!(pair.client_conn_mut(client_ch).accepted_0rtt());
    let server_ch = pair.server.assert_accept();

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(
        chunks.next(usize::MAX),
        Ok(Some(chunk)) if chunk.offset == 0 && chunk.bytes == MSG
    );
    let _ = chunks.finalize();
    assert_eq!(pair.client_conn_mut(client_ch).lost_packets(), 0);
}

#[test]
fn zero_rtt_rejection() {
    let _guard = subscribe();
    let mut server_config = server_config();
    Arc::get_mut(&mut server_config.crypto)
        .unwrap()
        .set_protocols(&["foo".into(), "bar".into()]);
    let mut pair = Pair::new(Arc::new(EndpointConfig::default()), server_config);
    let mut client_config = client_config();
    Arc::get_mut(&mut client_config.crypto)
        .unwrap()
        .set_protocols(&["foo".into()]);

    // Establish normal connection
    let client_ch = pair.begin_connect(client_config.clone());
    pair.drive();
    let server_ch = pair.server.assert_accept();
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Connected)
    );
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);
    pair.client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .close(pair.time, VarInt(0), [][..].into());
    pair.drive();
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::ConnectionLost { .. })
    );
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);
    pair.client.connections.clear();
    pair.server.connections.clear();

    // Changing protocols invalidates 0-RTT
    Arc::get_mut(&mut client_config.crypto)
        .unwrap()
        .set_protocols(&["bar".into()]);
    info!("resuming session");
    let client_ch = pair.begin_connect(client_config);
    assert!(pair.client_conn_mut(client_ch).has_0rtt());
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    const MSG: &[u8] = b"Hello, 0-RTT!";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    pair.drive();
    assert!(!pair.client_conn_mut(client_ch).accepted_0rtt());
    let server_ch = pair.server.assert_accept();
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Connected)
    );
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);
    let s2 = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    assert_eq!(s, s2);

    let mut recv = pair.server_recv(server_ch, s2);
    let mut chunks = recv.read(false).unwrap();
    assert_eq!(chunks.next(usize::MAX), Err(ReadError::Blocked));
    let _ = chunks.finalize();
    assert_eq!(pair.client_conn_mut(client_ch).lost_packets(), 0);
}

#[test]
fn alpn_success() {
    let _guard = subscribe();
    let mut server_config = server_config();
    Arc::get_mut(&mut server_config.crypto)
        .unwrap()
        .set_protocols(&["foo".into(), "bar".into(), "baz".into()]);
    let mut pair = Pair::new(Arc::new(EndpointConfig::default()), server_config);
    let mut client_config = client_config();
    Arc::get_mut(&mut client_config.crypto)
        .unwrap()
        .set_protocols(&["bar".into(), "quux".into(), "corge".into()]);

    // Establish normal connection
    let client_ch = pair.begin_connect(client_config);
    pair.drive();
    let server_ch = pair.server.assert_accept();
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Connected)
    );

    let hd = pair
        .client_conn_mut(client_ch)
        .crypto_session()
        .handshake_data()
        .unwrap();
    assert_eq!(hd.protocol.unwrap(), &b"bar"[..]);
}

#[test]
fn server_alpn_unset() {
    let _guard = subscribe();
    let mut pair = Pair::new(Arc::new(EndpointConfig::default()), server_config());
    let mut client_config = client_config();
    Arc::get_mut(&mut client_config.crypto)
        .unwrap()
        .set_protocols(&["foo".into()]);

    let client_ch = pair.begin_connect(client_config);
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::ConnectionLost { reason: ConnectionError::TransportError(ref err) }) if err.code == TransportErrorCode::crypto(0x78)
    );
}

#[test]
fn client_alpn_unset() {
    let _guard = subscribe();
    let mut server_config = server_config();
    Arc::get_mut(&mut server_config.crypto)
        .unwrap()
        .set_protocols(&["foo".into(), "bar".into(), "baz".into()]);
    let mut pair = Pair::new(Arc::new(EndpointConfig::default()), server_config);

    let client_ch = pair.begin_connect(client_config());
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::ConnectionLost { reason: ConnectionError::ConnectionClosed(err) }) if err.error_code == TransportErrorCode::crypto(0x78)
    );
}

#[test]
fn alpn_mismatch() {
    let mut server_config = server_config();
    Arc::get_mut(&mut server_config.crypto)
        .unwrap()
        .set_protocols(&["foo".into(), "bar".into(), "baz".into()]);
    let mut pair = Pair::new(Arc::new(EndpointConfig::default()), server_config);
    let mut client_config = client_config();
    Arc::get_mut(&mut client_config.crypto)
        .unwrap()
        .set_protocols(&["quux".into(), "corge".into()]);

    let client_ch = pair.begin_connect(client_config);
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::ConnectionLost { reason: ConnectionError::ConnectionClosed(err) }) if err.error_code == TransportErrorCode::crypto(0x78)
    );
}

#[test]
fn stream_id_limit() {
    let _guard = subscribe();
    let server = ServerConfig {
        transport: Arc::new(TransportConfig {
            max_concurrent_uni_streams: 1u32.into(),
            ..TransportConfig::default()
        }),
        ..server_config()
    };
    let mut pair = Pair::new(Default::default(), server);
    let (client_ch, server_ch) = pair.connect();

    let s = pair
        .client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .streams()
        .open(Dir::Uni)
        .expect("couldn't open first stream");
    assert_eq!(
        pair.client_streams(client_ch).open(Dir::Uni),
        None,
        "only one stream is permitted at a time"
    );
    // Generate some activity to allow the server to see the stream
    const MSG: &[u8] = b"hello";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    pair.client_send(client_ch, s).finish().unwrap();
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Stream(StreamEvent::Finished { id })) if id == s
    );
    assert_eq!(
        pair.client_streams(client_ch).open(Dir::Uni),
        None,
        "server does not immediately grant additional credit"
    );
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Uni }))
    );
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(
        chunks.next(usize::MAX),
        Ok(Some(chunk)) if chunk.offset == 0 && chunk.bytes == MSG
    );
    assert_eq!(chunks.next(usize::MAX), Ok(None));
    let _ = chunks.finalize();

    // Server will only send MAX_STREAM_ID now that the application's been notified
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Stream(StreamEvent::Available { dir: Dir::Uni }))
    );
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);

    // Try opening the second stream again, now that we've made room
    let s = pair
        .client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .streams()
        .open(Dir::Uni)
        .expect("didn't get stream id budget");
    pair.client_send(client_ch, s).finish().unwrap();
    pair.drive();
    // Make sure the server actually processes data on the newly-available stream
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Uni }))
    );
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(chunks.next(usize::MAX), Ok(None));
    let _ = chunks.finalize();
}

#[test]
fn key_update_simple() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();
    let s = pair
        .client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .streams()
        .open(Dir::Bi)
        .expect("couldn't open first stream");

    const MSG1: &[u8] = b"hello1";
    pair.client_send(client_ch, s).write(MSG1).unwrap();
    pair.drive();

    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Bi }))
    );
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Bi), Some(stream) if stream == s);
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);
    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(
        chunks.next(usize::MAX),
        Ok(Some(chunk)) if chunk.offset == 0 && chunk.bytes == MSG1
    );
    let _ = chunks.finalize();

    info!("initiating key update");
    pair.client_conn_mut(client_ch).initiate_key_update();

    const MSG2: &[u8] = b"hello2";
    pair.client_send(client_ch, s).write(MSG2).unwrap();
    pair.drive();

    assert_matches!(pair.server_conn_mut(server_ch).poll(), Some(Event::Stream(StreamEvent::Readable { id })) if id == s);
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);
    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(
        chunks.next(usize::MAX),
        Ok(Some(chunk)) if chunk.offset == 6 && chunk.bytes == MSG2
    );
    let _ = chunks.finalize();

    assert_eq!(pair.client_conn_mut(client_ch).lost_packets(), 0);
    assert_eq!(pair.server_conn_mut(server_ch).lost_packets(), 0);
}

#[test]
fn key_update_reordered() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();
    let s = pair
        .client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .streams()
        .open(Dir::Bi)
        .expect("couldn't open first stream");

    const MSG1: &[u8] = b"1";
    pair.client_send(client_ch, s).write(MSG1).unwrap();
    pair.client.drive(pair.time, pair.server.addr);
    assert!(!pair.client.outbound.is_empty());
    pair.client.delay_outbound();

    pair.client_conn_mut(client_ch).initiate_key_update();
    info!("updated keys");

    const MSG2: &[u8] = b"two";
    pair.client_send(client_ch, s).write(MSG2).unwrap();
    pair.client.drive(pair.time, pair.server.addr);
    pair.client.finish_delay();
    pair.drive();

    assert_eq!(pair.client_conn_mut(client_ch).lost_packets(), 0);
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Bi }))
    );
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Bi), Some(stream) if stream == s);

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(true).unwrap();
    let buf1 = chunks.next(usize::MAX).unwrap().unwrap();
    assert_matches!(&*buf1.bytes, MSG1);
    let buf2 = chunks.next(usize::MAX).unwrap().unwrap();
    assert_eq!(buf2.bytes, MSG2);
    let _ = chunks.finalize();

    assert_eq!(pair.client_conn_mut(client_ch).lost_packets(), 0);
    assert_eq!(pair.server_conn_mut(server_ch).lost_packets(), 0);
}

#[test]
fn initial_retransmit() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let client_ch = pair.begin_connect(client_config());
    pair.client.drive(pair.time, pair.server.addr);
    pair.client.outbound.clear(); // Drop initial
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Connected { .. })
    );
}

#[test]
fn instant_close_1() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    info!("connecting");
    let client_ch = pair.begin_connect(client_config());
    pair.client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .close(pair.time, VarInt(0), Bytes::new());
    pair.drive();
    let server_ch = pair.server.assert_accept();
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::ConnectionLost {
            reason: ConnectionError::ConnectionClosed(ConnectionClose {
                error_code: TransportErrorCode::APPLICATION_ERROR,
                ..
            }),
        })
    );
}

#[test]
fn instant_close_2() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    info!("connecting");
    let client_ch = pair.begin_connect(client_config());
    // Unlike `instant_close`, the server sees a valid Initial packet first.
    pair.drive_client();
    pair.client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .close(pair.time, VarInt(42), Bytes::new());
    pair.drive();
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);
    let server_ch = pair.server.assert_accept();
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::ConnectionLost {
            reason: ConnectionError::ConnectionClosed(ConnectionClose {
                error_code: TransportErrorCode::APPLICATION_ERROR,
                ..
            }),
        })
    );
}

#[test]
fn idle_timeout() {
    let _guard = subscribe();
    const IDLE_TIMEOUT: Duration = Duration::from_millis(100);
    let server = ServerConfig {
        transport: Arc::new(TransportConfig {
            max_idle_timeout: Some(IDLE_TIMEOUT),
            ..TransportConfig::default()
        }),
        ..server_config()
    };
    let mut pair = Pair::new(Default::default(), server);
    let (client_ch, server_ch) = pair.connect();
    pair.client_conn_mut(client_ch).ping();
    let start = pair.time;

    while !pair.client_conn_mut(client_ch).is_closed()
        || !pair.server_conn_mut(server_ch).is_closed()
    {
        if !pair.step() {
            if let Some(t) = min_opt(pair.client.next_wakeup(), pair.server.next_wakeup()) {
                pair.time = t;
            }
        }
        pair.client.inbound.clear(); // Simulate total S->C packet loss
    }

    assert!(pair.time - start < 2 * IDLE_TIMEOUT);
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::ConnectionLost {
            reason: ConnectionError::TimedOut,
        })
    );
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::ConnectionLost {
            reason: ConnectionError::TimedOut,
        })
    );
}

#[test]
fn connection_close_sends_acks() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, _server_ch) = pair.connect();

    let client_acks = pair.client_conn_mut(client_ch).stats().frame_rx.acks;

    pair.client_conn_mut(client_ch).ping();
    pair.drive_client();

    let time = pair.time;
    pair.server_conn_mut(client_ch)
        .close(time, VarInt(42), Bytes::new());

    pair.drive();

    let client_acks_2 = pair.client_conn_mut(client_ch).stats().frame_rx.acks;
    assert!(
        client_acks_2 > client_acks,
        "Connection close should send pending ACKs"
    );
}

#[test]
fn concurrent_connections_full() {
    let _guard = subscribe();
    let mut pair = Pair::new(
        Default::default(),
        ServerConfig {
            concurrent_connections: 0,
            ..server_config()
        },
    );
    let client_ch = pair.begin_connect(client_config());
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::ConnectionLost {
            reason: ConnectionError::ConnectionClosed(frame::ConnectionClose {
                error_code: TransportErrorCode::CONNECTION_REFUSED,
                ..
            }),
        })
    );
    assert_eq!(pair.server.connections.len(), 0);
    assert_eq!(pair.server.known_connections(), 0);
    assert_eq!(pair.server.known_cids(), 0);
}

#[test]
fn server_hs_retransmit() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let client_ch = pair.begin_connect(client_config());
    pair.step();
    assert!(!pair.client.inbound.is_empty()); // Initial + Handshakes
    pair.client.inbound.clear();
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Connected { .. })
    );
}

#[test]
fn migration() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();
    pair.client.addr = SocketAddr::new(
        Ipv4Addr::new(127, 0, 0, 1).into(),
        CLIENT_PORTS.lock().unwrap().next().unwrap(),
    );
    pair.client_conn_mut(client_ch).ping();

    // Assert that just receiving the ping message is accounted into the servers
    // anti-amplification budget
    pair.drive_client();
    pair.drive_server();
    assert_ne!(pair.server_conn_mut(server_ch).total_recvd(), 0);

    pair.drive();
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);
    assert_eq!(
        pair.server_conn_mut(server_ch).remote_address(),
        pair.client.addr
    );
}

fn test_flow_control(config: TransportConfig, window_size: usize) {
    let _guard = subscribe();
    let mut pair = Pair::new(
        Default::default(),
        ServerConfig {
            transport: Arc::new(config),
            ..server_config()
        },
    );
    let (client_ch, server_ch) = pair.connect();
    let msg = vec![0xAB; window_size + 10];

    // Stream reset before read
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    info!("writing");
    assert_eq!(pair.client_send(client_ch, s).write(&msg), Ok(window_size));
    assert_eq!(
        pair.client_send(client_ch, s).write(&msg[window_size..]),
        Err(WriteError::Blocked)
    );
    pair.drive();
    info!("resetting");
    pair.client_send(client_ch, s).reset(VarInt(42)).unwrap();
    pair.drive();

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(true).unwrap();
    assert_eq!(
        chunks.next(usize::MAX).err(),
        Some(ReadError::Reset(VarInt(42)))
    );
    let _ = chunks.finalize();

    // Happy path
    info!("writing");
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    assert_eq!(pair.client_send(client_ch, s).write(&msg), Ok(window_size));
    assert_eq!(
        pair.client_send(client_ch, s).write(&msg[window_size..]),
        Err(WriteError::Blocked)
    );

    pair.drive();
    let mut cursor = 0;
    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(true).unwrap();
    loop {
        match chunks.next(usize::MAX) {
            Ok(Some(chunk)) => {
                cursor += chunk.bytes.len();
            }
            Ok(None) => {
                panic!("end of stream");
            }
            Err(ReadError::Blocked) => {
                break;
            }
            Err(e) => {
                panic!("{}", e);
            }
        }
    }
    let _ = chunks.finalize();

    info!("finished reading");
    assert_eq!(cursor, window_size);
    pair.drive();
    info!("writing");
    assert_eq!(pair.client_send(client_ch, s).write(&msg), Ok(window_size));
    assert_eq!(
        pair.client_send(client_ch, s).write(&msg[window_size..]),
        Err(WriteError::Blocked)
    );

    pair.drive();
    let mut cursor = 0;
    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(true).unwrap();
    loop {
        match chunks.next(usize::MAX) {
            Ok(Some(chunk)) => {
                cursor += chunk.bytes.len();
            }
            Ok(None) => {
                panic!("end of stream");
            }
            Err(ReadError::Blocked) => {
                break;
            }
            Err(e) => {
                panic!("{}", e);
            }
        }
    }
    assert_eq!(cursor, window_size);
    let _ = chunks.finalize();
    info!("finished reading");
}

#[test]
fn stream_flow_control() {
    test_flow_control(
        TransportConfig {
            stream_receive_window: 2000u32.into(),
            ..TransportConfig::default()
        },
        2000,
    );
}

#[test]
fn conn_flow_control() {
    test_flow_control(
        TransportConfig {
            receive_window: 2000u32.into(),
            ..TransportConfig::default()
        },
        2000,
    );
}

#[test]
fn stop_opens_bidi() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();
    assert_eq!(pair.client_streams(client_ch).send_streams(), 0);
    let s = pair.client_streams(client_ch).open(Dir::Bi).unwrap();
    assert_eq!(pair.client_streams(client_ch).send_streams(), 1);
    const ERROR: VarInt = VarInt(42);
    pair.client
        .connections
        .get_mut(&server_ch)
        .unwrap()
        .recv_stream(s)
        .stop(ERROR)
        .unwrap();
    pair.drive();

    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Bi }))
    );
    assert_eq!(pair.server_conn_mut(client_ch).streams().send_streams(), 0);
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Bi), Some(stream) if stream == s);
    assert_eq!(pair.server_conn_mut(client_ch).streams().send_streams(), 1);

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(chunks.next(usize::MAX), Err(ReadError::Blocked));
    let _ = chunks.finalize();

    assert_matches!(
        pair.server_send(server_ch, s).write(b"foo"),
        Err(WriteError::Stopped(ERROR))
    );
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Stopped {
            id: _,
            error_code: ERROR
        }))
    );
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);
}

#[test]
fn implicit_open() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();
    let s1 = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    let s2 = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    pair.client_send(client_ch, s2).write(b"hello").unwrap();
    pair.drive();
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Uni }))
    );
    assert_eq!(pair.server_streams(server_ch).accept(Dir::Uni), Some(s1));
    assert_eq!(pair.server_streams(server_ch).accept(Dir::Uni), Some(s2));
    assert_eq!(pair.server_streams(server_ch).accept(Dir::Uni), None);
}

#[test]
fn zero_length_cid() {
    let _guard = subscribe();
    let cid_generator_factory: fn() -> Box<dyn ConnectionIdGenerator> =
        || Box::new(RandomConnectionIdGenerator::new(0));
    let mut pair = Pair::new(
        Arc::new(EndpointConfig {
            connection_id_generator_factory: Arc::new(cid_generator_factory),
            ..EndpointConfig::default()
        }),
        server_config(),
    );
    let (client_ch, server_ch) = pair.connect();
    // Ensure we can reconnect after a previous connection is cleaned up
    info!("closing");
    pair.client
        .connections
        .get_mut(&client_ch)
        .unwrap()
        .close(pair.time, VarInt(42), Bytes::new());
    pair.drive();
    pair.server
        .connections
        .get_mut(&server_ch)
        .unwrap()
        .close(pair.time, VarInt(42), Bytes::new());
    pair.connect();
}

#[test]
fn keep_alive() {
    let _guard = subscribe();
    const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
    let server = ServerConfig {
        transport: Arc::new(TransportConfig {
            keep_alive_interval: Some(IDLE_TIMEOUT / 2),
            max_idle_timeout: Some(IDLE_TIMEOUT),
            ..TransportConfig::default()
        }),
        ..server_config()
    };
    let mut pair = Pair::new(Default::default(), server);
    let (client_ch, server_ch) = pair.connect();
    // Run a good while longer than the idle timeout
    let end = pair.time + 20 * IDLE_TIMEOUT;
    while pair.time < end {
        if !pair.step() {
            if let Some(time) = min_opt(pair.client.next_wakeup(), pair.server.next_wakeup()) {
                pair.time = time;
            }
        }
        assert!(!pair.client_conn_mut(client_ch).is_closed());
        assert!(!pair.server_conn_mut(server_ch).is_closed());
    }
}

#[test]
fn cid_rotation() {
    let _guard = subscribe();
    const CID_TIMEOUT: Duration = Duration::from_secs(2);

    let cid_generator_factory: fn() -> Box<dyn ConnectionIdGenerator> =
        || Box::new(*RandomConnectionIdGenerator::new(8).set_lifetime(CID_TIMEOUT));

    // Only test cid rotation on server side to have a clear output trace
    let server = Endpoint::new(
        Arc::new(EndpointConfig {
            connection_id_generator_factory: Arc::new(cid_generator_factory),
            ..EndpointConfig::default()
        }),
        Some(Arc::new(server_config())),
    );
    let client = Endpoint::new(Arc::new(EndpointConfig::default()), None);

    let mut pair = Pair::new_from_endpoint(client, server);
    let (_, server_ch) = pair.connect();

    let mut round: u64 = 1;
    let mut stop = pair.time;
    let end = pair.time + 5 * CID_TIMEOUT;

    use crate::cid_queue::CidQueue;
    use crate::LOC_CID_COUNT;
    let mut active_cid_num = CidQueue::LEN as u64 + 1;
    active_cid_num = active_cid_num.min(LOC_CID_COUNT);
    let mut left_bound = 0;
    let mut right_bound = active_cid_num - 1;

    while pair.time < end {
        stop += CID_TIMEOUT;
        // Run a while until PushNewCID timer fires
        while pair.time < stop {
            if !pair.step() {
                if let Some(time) = min_opt(pair.client.next_wakeup(), pair.server.next_wakeup()) {
                    pair.time = time;
                }
            }
        }
        info!(
            "Checking active cid sequence range before {:?} seconds",
            round * CID_TIMEOUT.as_secs()
        );
        let _bound = (left_bound, right_bound);
        assert_matches!(
            pair.server_conn_mut(server_ch).active_local_cid_seq(),
            _bound
        );
        round += 1;
        left_bound += active_cid_num;
        right_bound += active_cid_num;
        pair.drive_server();
    }
}

#[test]
fn cid_retirement() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    // Server retires current active remote CIDs
    pair.server_conn_mut(server_ch)
        .rotate_local_cid(1, Instant::now());
    pair.drive();
    // Any unexpected behavior may trigger TransportError::CONNECTION_ID_LIMIT_ERROR
    assert!(!pair.client_conn_mut(client_ch).is_closed());
    assert!(!pair.server_conn_mut(server_ch).is_closed());
    assert_matches!(pair.client_conn_mut(client_ch).active_rem_cid_seq(), 1);

    use crate::cid_queue::CidQueue;
    use crate::LOC_CID_COUNT;
    let mut active_cid_num = CidQueue::LEN as u64;
    active_cid_num = active_cid_num.min(LOC_CID_COUNT);

    let next_retire_prior_to = active_cid_num + 1;
    pair.client_conn_mut(client_ch).ping();
    // Server retires all valid remote CIDs
    pair.server_conn_mut(server_ch)
        .rotate_local_cid(next_retire_prior_to, Instant::now());
    pair.drive();
    assert!(!pair.client_conn_mut(client_ch).is_closed());
    assert!(!pair.server_conn_mut(server_ch).is_closed());
    assert_matches!(
        pair.client_conn_mut(client_ch).active_rem_cid_seq(),
        _next_retire_prior_to
    );
}

#[test]
fn finish_stream_flow_control_reordered() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();

    const MSG: &[u8] = b"hello";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    pair.drive_client(); // Send stream data
    pair.server.drive(pair.time, pair.client.addr); // Receive

    // Issue flow control credit
    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(
        chunks.next(usize::MAX),
        Ok(Some(chunk)) if chunk.offset == 0 && chunk.bytes == MSG
    );
    let _ = chunks.finalize();

    pair.server.drive(pair.time, pair.client.addr);
    pair.server.delay_outbound(); // Delay it

    pair.client_send(client_ch, s).finish().unwrap();
    pair.drive_client(); // Send FIN
    pair.server.drive(pair.time, pair.client.addr); // Acknowledge
    pair.server.finish_delay(); // Add flow control packets after
    pair.drive();

    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Stream(StreamEvent::Finished { id })) if id == s
    );
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Uni }))
    );
    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(chunks.next(usize::MAX), Ok(None));
    let _ = chunks.finalize();
}

#[test]
fn handshake_1rtt_handling() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let client_ch = pair.begin_connect(client_config());
    pair.drive_client();
    pair.drive_server();
    let server_ch = pair.server.assert_accept();
    // Server now has 1-RTT keys, but remains in Handshake state until the TLS CFIN has
    // authenticated the client. Delay the final client handshake flight so that doesn't happen yet.
    pair.client.drive(pair.time, pair.server.addr);
    pair.client.delay_outbound();

    // Send some 1-RTT data which will be received first.
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    const MSG: &[u8] = b"hello";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    pair.client_send(client_ch, s).finish().unwrap();
    pair.client.drive(pair.time, pair.server.addr);

    // Add the handshake flight back on.
    pair.client.finish_delay();

    pair.drive();

    assert!(pair.client_conn_mut(client_ch).lost_packets() != 0);
    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(
        chunks.next(usize::MAX),
        Ok(Some(chunk)) if chunk.offset == 0 && chunk.bytes == MSG
    );
    let _ = chunks.finalize();
}

#[test]
fn stop_before_finish() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    const MSG: &[u8] = b"hello";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    pair.drive();

    info!("stopping stream");
    const ERROR: VarInt = VarInt(42);
    pair.server_recv(server_ch, s).stop(ERROR).unwrap();
    pair.drive();

    assert_matches!(
        pair.client_send(client_ch, s).finish(),
        Err(FinishError::Stopped(ERROR))
    );
}

#[test]
fn stop_during_finish() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    const MSG: &[u8] = b"hello";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    pair.drive();

    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);
    info!("stopping and finishing stream");
    const ERROR: VarInt = VarInt(42);
    pair.server_recv(server_ch, s).stop(ERROR).unwrap();
    pair.drive_server();
    pair.client_send(client_ch, s).finish().unwrap();
    pair.drive_client();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Stream(StreamEvent::Stopped { id, error_code: ERROR })) if id == s
    );
}

// Ensure we can recover from loss of tail packets when the congestion window is full
#[test]
fn congested_tail_loss() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, _) = pair.connect();

    const TARGET: u64 = 2048;
    assert!(pair.client_conn_mut(client_ch).congestion_window() > TARGET);
    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();
    // Send data without receiving ACKs until the congestion state falls below target
    while pair.client_conn_mut(client_ch).congestion_window() > TARGET {
        let n = pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
        assert_eq!(n, 1024);
        pair.drive_client();
    }
    assert!(!pair.server.inbound.is_empty());
    pair.server.inbound.clear();
    // Ensure that the congestion state recovers after retransmits occur and are ACKed
    info!("recovering");
    pair.drive();
    assert!(pair.client_conn_mut(client_ch).congestion_window() > TARGET);
    pair.client_send(client_ch, s).write(&[42; 1024]).unwrap();
}

#[test]
fn datagram_send_recv() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);
    assert_matches!(pair.client_datagrams(client_ch).max_size(), Some(x) if x > 0);

    const DATA: &[u8] = b"whee";
    pair.client_datagrams(client_ch).send(DATA.into()).unwrap();
    pair.drive();
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::DatagramReceived)
    );
    assert_eq!(pair.server_datagrams(server_ch).recv().unwrap(), DATA);
    assert_matches!(pair.server_datagrams(server_ch).recv(), None);
}

#[test]
fn datagram_recv_buffer_overflow() {
    let _guard = subscribe();
    const WINDOW: usize = 100;
    let server = ServerConfig {
        transport: Arc::new(TransportConfig {
            datagram_receive_buffer_size: Some(WINDOW),
            ..TransportConfig::default()
        }),
        ..server_config()
    };
    let mut pair = Pair::new(Default::default(), server);
    let (client_ch, server_ch) = pair.connect();
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);
    assert_matches!(
        pair.client_conn_mut(client_ch).datagrams().max_size(),
        Some(WINDOW)
    );

    const DATA1: &[u8] = &[0xAB; (WINDOW / 3) + 1];
    const DATA2: &[u8] = &[0xBC; (WINDOW / 3) + 1];
    const DATA3: &[u8] = &[0xCD; (WINDOW / 3) + 1];
    pair.client_datagrams(client_ch).send(DATA1.into()).unwrap();
    pair.client_datagrams(client_ch).send(DATA2.into()).unwrap();
    pair.client_datagrams(client_ch).send(DATA3.into()).unwrap();
    pair.drive();
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::DatagramReceived)
    );
    assert_eq!(pair.server_datagrams(server_ch).recv().unwrap(), DATA2);
    assert_eq!(pair.server_datagrams(server_ch).recv().unwrap(), DATA3);
    assert_matches!(pair.server_datagrams(server_ch).recv(), None);

    pair.client_datagrams(client_ch).send(DATA1.into()).unwrap();
    pair.drive();
    assert_eq!(pair.server_datagrams(server_ch).recv().unwrap(), DATA1);
    assert_matches!(pair.server_datagrams(server_ch).recv(), None);
}

#[test]
fn datagram_unsupported() {
    let _guard = subscribe();
    let server = ServerConfig {
        transport: Arc::new(TransportConfig {
            datagram_receive_buffer_size: None,
            ..TransportConfig::default()
        }),
        ..server_config()
    };
    let mut pair = Pair::new(Default::default(), server);
    let (client_ch, server_ch) = pair.connect();
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);
    assert_matches!(pair.client_datagrams(client_ch).max_size(), None);

    match pair.client_datagrams(client_ch).send(Bytes::new()) {
        Err(SendDatagramError::UnsupportedByPeer) => {}
        Err(e) => panic!("unexpected error: {}", e),
        Ok(_) => panic!("unexpected success"),
    }
}

#[test]
fn large_initial() {
    let _guard = subscribe();
    let mut server_config = server_config();
    Arc::get_mut(&mut server_config.crypto)
        .unwrap()
        .set_protocols(&[vec![0, 0, 0, 42]]);
    let mut pair = Pair::new(Arc::new(EndpointConfig::default()), server_config);
    let mut cfg = client_config();
    let protocols = (0..1000u32)
        .map(|x| x.to_be_bytes().to_vec())
        .collect::<Vec<_>>();
    Arc::get_mut(&mut cfg.crypto)
        .unwrap()
        .set_protocols(&protocols);
    let client_ch = pair.begin_connect(cfg);
    pair.drive();
    let server_ch = pair.server.assert_accept();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Connected { .. })
    );
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Connected { .. })
    );
}

#[test]
/// Ensure that we don't yield a finish event before the actual FIN is acked so the peer isn't left
/// hanging
fn finish_acked() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();

    const MSG: &[u8] = b"hello";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    info!("client sends data to server");
    pair.drive_client(); // send data to server
    info!("server acknowledges data");
    pair.drive_server(); // process data and send data ack

    // Receive data
    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Uni }))
    );
    assert_matches!(pair.server_conn_mut(server_ch).poll(), None);

    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(
        chunks.next(usize::MAX),
        Ok(Some(chunk)) if chunk.offset == 0 && chunk.bytes == MSG
    );
    assert_matches!(chunks.next(usize::MAX), Err(ReadError::Blocked));
    let _ = chunks.finalize();

    // Finish before receiving data ack
    pair.client_send(client_ch, s).finish().unwrap();
    // Send FIN, receive data ack
    info!("client receives ACK, sends FIN");
    pair.drive_client();
    // Check for premature finish from data ack
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);
    // Process FIN ack
    info!("server ACKs FIN");
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Stream(StreamEvent::Finished { id })) if id == s
    );

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(chunks.next(usize::MAX), Ok(None));
    let _ = chunks.finalize();
}

#[test]
/// Ensure that we don't yield a finish event while there's still unacknowledged data
fn finish_retransmit() {
    let _guard = subscribe();
    let mut pair = Pair::default();
    let (client_ch, server_ch) = pair.connect();

    let s = pair.client_streams(client_ch).open(Dir::Uni).unwrap();

    const MSG: &[u8] = b"hello";
    pair.client_send(client_ch, s).write(MSG).unwrap();
    pair.drive_client(); // send data to server
    pair.server.inbound.clear(); // Lose it

    // Send FIN
    pair.client_send(client_ch, s).finish().unwrap();
    pair.drive_client();
    // Process FIN
    pair.drive_server();
    // Receive FIN ack, but no data ack
    pair.drive_client();
    // Check for premature finish from FIN ack
    assert_matches!(pair.client_conn_mut(client_ch).poll(), None);
    // Recover
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Stream(StreamEvent::Finished { id })) if id == s
    );

    assert_matches!(
        pair.server_conn_mut(server_ch).poll(),
        Some(Event::Stream(StreamEvent::Opened { dir: Dir::Uni }))
    );

    assert_matches!(pair.server_streams(server_ch).accept(Dir::Uni), Some(stream) if stream == s);

    let mut recv = pair.server_recv(server_ch, s);
    let mut chunks = recv.read(false).unwrap();
    assert_matches!(
        chunks.next(usize::MAX),
        Ok(Some(chunk)) if chunk.offset == 0 && chunk.bytes == MSG
    );
    assert_matches!(chunks.next(usize::MAX), Ok(None));
    let _ = chunks.finalize();
}

/// Ensures that exchanging data on a client-initiated bidirectional stream works past the initial
/// stream window.
#[test]
fn repeated_request_response() {
    let _guard = subscribe();
    let server = ServerConfig {
        transport: Arc::new(TransportConfig {
            max_concurrent_bidi_streams: 1u32.into(),
            ..TransportConfig::default()
        }),
        ..server_config()
    };
    let mut pair = Pair::new(Default::default(), server);
    let (client_ch, server_ch) = pair.connect();
    const REQUEST: &[u8] = b"hello";
    const RESPONSE: &[u8] = b"world";
    for _ in 0..3 {
        let s = pair.client_streams(client_ch).open(Dir::Bi).unwrap();

        pair.client_send(client_ch, s).write(REQUEST).unwrap();
        pair.client_send(client_ch, s).finish().unwrap();

        pair.drive();

        assert_eq!(pair.server_streams(server_ch).accept(Dir::Bi), Some(s));
        let mut recv = pair.server_recv(server_ch, s);
        let mut chunks = recv.read(false).unwrap();
        assert_matches!(
            chunks.next(usize::MAX),
            Ok(Some(chunk)) if chunk.offset == 0 && chunk.bytes == REQUEST
        );

        assert_matches!(chunks.next(usize::MAX), Ok(None));
        let _ = chunks.finalize();
        pair.server_send(server_ch, s).write(RESPONSE).unwrap();
        pair.server_send(server_ch, s).finish().unwrap();

        pair.drive();

        let mut recv = pair.client_recv(client_ch, s);
        let mut chunks = recv.read(false).unwrap();
        assert_matches!(
            chunks.next(usize::MAX),
            Ok(Some(chunk)) if chunk.offset == 0 && chunk.bytes == RESPONSE
        );
        assert_matches!(chunks.next(usize::MAX), Ok(None));
        let _ = chunks.finalize();
    }
}

/// Ensures that the client sends an anti-deadlock probe after an incomplete server's first flight
#[test]
fn handshake_anti_deadlock_probe() {
    let _guard = subscribe();

    let (cert, key) = big_cert_and_key();

    let mut server = server_config();
    server
        .certificate(CertificateChain::from_certs(Some(cert.clone())), key)
        .unwrap();
    let mut client = client_config();
    client.add_certificate_authority(cert).unwrap();
    let mut pair = Pair::new(Default::default(), server);

    let client_ch = pair.begin_connect(client);
    // Client sends initial
    pair.drive_client();
    // Server sends first flight, gets blocked on anti-amplification
    pair.drive_server();
    // Client acks...
    pair.drive_client();
    // ...but it's lost, so the server doesn't get anti-amplification credit from it
    pair.server.inbound.clear();
    // Client sends an anti-deadlock probe, and the handshake completes as usual.
    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Connected { .. })
    );
}

/// Ensures that the server can respond with 3 initial packets during the handshake
/// before the anti-amplification limit kicks in when MTUs are similar.
#[test]
fn server_can_send_3_inital_packets() {
    let _guard = subscribe();

    let (cert, key) = big_cert_and_key();

    let mut server = server_config();
    server
        .certificate(CertificateChain::from_certs(Some(cert.clone())), key)
        .unwrap();
    let mut client = client_config();
    client.add_certificate_authority(cert).unwrap();
    let mut pair = Pair::new(Default::default(), server);

    let client_ch = pair.begin_connect(client);
    // Client sends initial
    pair.drive_client();
    // Server sends first flight, gets blocked on anti-amplification
    pair.drive_server();
    // Server should have queued 3 packets at this time
    assert_eq!(pair.client.inbound.len(), 3);

    pair.drive();
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::HandshakeDataReady)
    );
    assert_matches!(
        pair.client_conn_mut(client_ch).poll(),
        Some(Event::Connected { .. })
    );
}

/// Generate a big fat certificate that can't fit inside the initial anti-amplification limit
fn big_cert_and_key() -> (Certificate, PrivateKey) {
    let cert = rcgen::generate_simple_self_signed(
        Some("localhost".into())
            .into_iter()
            .chain((0..1000).map(|x| format!("foo_{}", x)))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let key = PrivateKey::from_der(&cert.serialize_private_key_der()).unwrap();
    let cert = Certificate::from_der(&cert.serialize_der().unwrap()).unwrap();
    (cert, key)
}
