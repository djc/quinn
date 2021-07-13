#![cfg(feature = "rustls")]
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use crc::crc32;
use futures::{future, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use quinn::{ConnectionError, ReadError, WriteError};
use rand::{self, RngCore};
use tokio::runtime::Builder;
use unwrap::unwrap;

struct Shared {
    errors: Vec<ConnectionError>,
}

#[test]
#[ignore]
fn connect_n_nodes_to_1_and_send_1mb_data() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();

    let runtime = unwrap!(Builder::new_current_thread().enable_all().build());
    let shared = Arc::new(Mutex::new(Shared { errors: vec![] }));

    let (cfg, listener_cert) = configure_listener();
    let mut ep_builder = quinn::Endpoint::builder();
    ep_builder.listen(cfg);
    let (endpoint, incoming_conns) = {
        let _guard = runtime.enter();
        unwrap!(ep_builder.bind(&"127.0.0.1:0".parse().unwrap()))
    };
    let listener_addr = unwrap!(endpoint.local_addr());

    let expected_messages = 50;

    let shared2 = shared.clone();
    let read_incoming_data = incoming_conns
        .filter_map(|connect| connect.map(|x| x.ok()))
        .take(expected_messages)
        .for_each(move |new_conn| {
            let conn = new_conn.connection;

            let shared = shared2.clone();
            let task = new_conn
                .uni_streams
                .try_for_each(move |stream| {
                    let conn = conn.clone();
                    read_from_peer(stream).map(move |_| {
                        conn.close(0u32.into(), &[]);
                        Ok(())
                    })
                })
                .unwrap_or_else(move |e| {
                    shared.lock().unwrap().errors.push(e);
                });
            tokio::spawn(task);

            future::ready(())
        });
    runtime.spawn(read_incoming_data);

    let client_cfg = configure_connector(&listener_cert);

    for _ in 0..expected_messages {
        let data = random_data_with_hash(1024 * 1024);
        let shared = shared.clone();
        let task = unwrap!(endpoint.connect_with(client_cfg.clone(), &listener_addr, "localhost"))
            .map_err(WriteError::ConnectionClosed)
            .and_then(move |new_conn| write_to_peer(new_conn.connection, data))
            .unwrap_or_else(move |e| {
                use quinn::ConnectionError::*;
                match e {
                    WriteError::ConnectionClosed(ApplicationClosed { .. })
                    | WriteError::ConnectionClosed(Reset) => {}
                    WriteError::ConnectionClosed(e) => shared.lock().unwrap().errors.push(e),
                    _ => panic!("unexpected write error"),
                }
            });
        runtime.spawn(task);
    }

    runtime.block_on(endpoint.wait_idle());
    let shared = shared.lock().unwrap();
    if !shared.errors.is_empty() {
        panic!("some connections failed: {:?}", shared.errors);
    }
}

async fn read_from_peer(stream: quinn::RecvStream) -> Result<(), quinn::ConnectionError> {
    match stream.read_to_end(1024 * 1024 * 5).await {
        Ok(data) => {
            assert!(hash_correct(&data));
            Ok(())
        }
        Err(e) => {
            use quinn::ReadToEndError::*;
            use ReadError::*;
            match e {
                TooLong
                | Read(UnknownStream)
                | Read(ZeroRttRejected)
                | Read(IllegalOrderedRead) => unreachable!(),
                Read(Reset(error_code)) => panic!("unexpected stream reset: {}", error_code),
                Read(ConnectionClosed(e)) => Err(e),
            }
        }
    }
}

async fn write_to_peer(conn: quinn::Connection, data: Vec<u8>) -> Result<(), WriteError> {
    let mut s = conn
        .open_uni()
        .await
        .map_err(WriteError::ConnectionClosed)?;
    s.write_all(&data).await?;
    // Suppress finish errors, since the peer may close before ACKing
    match s.finish().await {
        Ok(()) => Ok(()),
        Err(WriteError::ConnectionClosed(ConnectionError::ApplicationClosed { .. })) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Builds client configuration. Trusts given node certificate.
fn configure_connector(node_cert: &[u8]) -> quinn::ClientConfig {
    let their_cert = unwrap!(quinn::Certificate::from_der(&node_cert));
    let mut peer_cfg = unwrap!(quinn::ClientConfig::with_root_certificates(
        vec![their_cert],
        None
    ));
    let transport_config = unwrap!(Arc::get_mut(&mut peer_cfg.transport));
    transport_config
        .max_idle_timeout(Some(Duration::from_secs(20)))
        .unwrap();

    peer_cfg
}

/// Builds listener configuration along with its certificate.
fn configure_listener() -> (quinn::ServerConfig, Vec<u8>) {
    let (our_cert_der, our_priv_key) = gen_cert();
    let our_cert = unwrap!(quinn::Certificate::from_der(&our_cert_der));
    let our_cert_chain = quinn::CertificateChain::from_certs(vec![our_cert]);
    let mut our_cfg = quinn::ServerConfig::with_single_cert(our_cert_chain, our_priv_key).unwrap();

    let transport_config = unwrap!(Arc::get_mut(&mut our_cfg.transport));
    transport_config
        .max_idle_timeout(Some(Duration::from_secs(20)))
        .unwrap();

    (our_cfg, our_cert_der)
}

fn gen_cert() -> (Vec<u8>, quinn::PrivateKey) {
    let cert = unwrap!(rcgen::generate_simple_self_signed(vec![
        "localhost".to_string()
    ]));
    let key = unwrap!(quinn::PrivateKey::from_der(
        &cert.serialize_private_key_der()
    ));
    (unwrap!(cert.serialize_der()), key)
}

/// Constructs a buffer with random bytes of given size prefixed with a hash of this data.
fn random_data_with_hash(size: usize) -> Vec<u8> {
    let mut data = random_vec(size + 4);
    let hash = crc32::checksum_ieee(&data[4..]);
    // write hash in big endian
    data[0] = (hash >> 24) as u8;
    data[1] = ((hash >> 16) & 0xff) as u8;
    data[2] = ((hash >> 8) & 0xff) as u8;
    data[3] = (hash & 0xff) as u8;
    data
}

/// Checks if given data buffer hash is correct. Hash itself is a 4 byte prefix in the data.
fn hash_correct(data: &[u8]) -> bool {
    let encoded_hash = ((data[0] as u32) << 24)
        | ((data[1] as u32) << 16)
        | ((data[2] as u32) << 8)
        | data[3] as u32;
    let actual_hash = crc32::checksum_ieee(&data[4..]);
    encoded_hash == actual_hash
}

#[allow(unsafe_code)]
fn random_vec(size: usize) -> Vec<u8> {
    let mut ret = Vec::with_capacity(size);
    unsafe { ret.set_len(size) };
    rand::thread_rng().fill_bytes(&mut ret[..]);
    ret
}
