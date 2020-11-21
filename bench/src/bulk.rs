use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use tokio::runtime::{Builder, Runtime};
use tracing::trace;
use winapi::um::winsock2;

const NR_ITERATIONS: usize = 100;
const NR_CHUNKS: usize = 2 * 1024;
const DATA_LEN: usize = 1 * 1024 * 1024;

fn main() {
    let mut winsock_data = winsock2::WSADATA::default();
    if unsafe { winsock2::WSAStartup(0x202, &mut winsock_data) } != 0 {
        panic!("Error starting winsock");
    }

    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = quinn::PrivateKey::from_der(&cert.serialize_private_key_der()).unwrap();
    let cert = quinn::Certificate::from_der(&cert.serialize_der().unwrap()).unwrap();

    let mut server_config = quinn::ServerConfigBuilder::default();
    server_config
        .certificate(quinn::CertificateChain::from_certs(vec![cert.clone()]), key)
        .unwrap();
    let mut endpoint = quinn::EndpointBuilder::default();
    endpoint.listen(server_config.build());
    let mut runtime = rt();
    let (endpoint, incoming) = runtime.enter(|| {
        endpoint
            .bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .unwrap()
    });
    let server_addr = endpoint.local_addr().unwrap();
    drop(endpoint); // Ensure server shuts down when finished
    let thread = std::thread::spawn(move || {
        if let Err(e) = runtime.block_on(server(incoming)) {
            eprintln!("server failed: {:#}", e);
        }
    });

    let mut runtime = rt();
    if let Err(e) = runtime.block_on(client(server_addr, cert)) {
        eprintln!("client failed: {:#}", e);
    }

    thread.join().expect("server thread");
}

async fn server(mut incoming: quinn::Incoming) -> Result<()> {
    let handshake = incoming.next().await.unwrap();
    let quinn::NewConnection {
        mut uni_streams, ..
    } = handshake.await.context("handshake failed")?;
    for _ in 0..NR_ITERATIONS {
        let mut stream = uni_streams
            .next()
            .await
            .ok_or_else(|| anyhow!("accepting stream failed"))??;
        trace!("stream established");
        let start = Instant::now();
        let mut n = 0;
        while let Some((data, offset)) = stream.read_unordered().await? {
            n = n.max(offset + data.len() as u64);
        }
        let dt = start.elapsed();
        println!(
            "recvd {} bytes in {:?} ({} MiB/s)",
            n,
            dt,
            n as f32 / (dt.as_secs_f32() * 1024.0 * 1024.0)
        );
    }
    Ok(())
}

async fn client(server_addr: SocketAddr, server_cert: quinn::Certificate) -> Result<()> {
    let (endpoint, _) = quinn::EndpointBuilder::default()
        .bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .unwrap();

    let mut client_config = quinn::ClientConfigBuilder::default();
    client_config
        .add_certificate_authority(server_cert)
        .unwrap();
    let quinn::NewConnection { connection, .. } = endpoint
        .connect_with(client_config.build(), &server_addr, "localhost")
        .unwrap()
        .await
        .context("unable to connect")?;
    trace!("connected");

    for _ in 0..NR_ITERATIONS {
        let mut stream = connection
            .open_uni()
            .await
            .context("failed to open stream")?;
        const DATA: &[u8] = &[0xAB; DATA_LEN];
        let start = Instant::now();
        for _ in 0..NR_CHUNKS {
            stream
                .write_all(DATA)
                .await
                .context("failed sending data")?;
        }
        stream.finish().await.context("failed finishing stream")?;
        let dt = start.elapsed();
        println!(
            "sent {} bytes in {:?} ({} MiB/s)",
            1024 * DATA.len(),
            dt,
            (NR_CHUNKS * DATA_LEN) as f32 / 1024.0 / 1024.0 / dt.as_secs_f32()
        );
    }
    Ok(())
}

fn rt() -> Runtime {
    Builder::new()
        .basic_scheduler()
        .enable_all()
        .build()
        .unwrap()
}
