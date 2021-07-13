use std::{fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use structopt::StructOpt;
use tracing::{debug, error, info};

use perf::bind_socket;

#[derive(StructOpt)]
#[structopt(name = "server")]
struct Opt {
    /// Address to listen on
    #[structopt(long = "listen", default_value = "[::]:4433")]
    listen: SocketAddr,
    /// TLS private key in PEM format
    #[structopt(parse(from_os_str), short = "k", long = "key", requires = "cert")]
    key: Option<PathBuf>,
    /// TLS certificate in PEM format
    #[structopt(parse(from_os_str), short = "c", long = "cert", requires = "key")]
    cert: Option<PathBuf>,
    /// Send buffer size in bytes
    #[structopt(long, default_value = "2097152")]
    send_buffer_size: usize,
    /// Receive buffer size in bytes
    #[structopt(long, default_value = "2097152")]
    recv_buffer_size: usize,
    /// Whether to print connection statistics
    #[structopt(long)]
    conn_stats: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let opt = Opt::from_args();

    tracing_subscriber::fmt::init();

    if let Err(e) = run(opt).await {
        error!("{:#}", e);
    }
}

async fn run(opt: Opt) -> Result<()> {
    let (key, cert) = match (&opt.key, &opt.cert) {
        (&Some(ref key), &Some(ref cert)) => {
            let key = fs::read(key).context("reading key")?;
            let cert = fs::read(cert).expect("reading cert");

            let mut certs = Vec::new();
            for cert in rustls_pemfile::certs(&mut cert.as_ref()).context("parsing cert")? {
                certs.push(rustls::Certificate(cert));
            }

            (rustls::PrivateKey(key), certs)
        }
        _ => {
            let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            (
                rustls::PrivateKey(cert.serialize_private_key_der()),
                vec![rustls::Certificate(cert.serialize_der().unwrap())],
            )
        }
    };

    let mut crypto = rustls::ServerConfig::builder()
        .with_cipher_suites(PERF_CIPHER_SUITES)
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(cert, key)
        .unwrap();
    crypto.alpn_protocols = vec![b"perf".to_vec()];

    let server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let mut endpoint = quinn::EndpointBuilder::default();
    endpoint.listen(server_config);

    let socket = bind_socket(opt.listen, opt.send_buffer_size, opt.recv_buffer_size)?;

    let (endpoint, mut incoming) = endpoint.with_socket(socket).context("creating endpoint")?;

    info!("listening on {}", endpoint.local_addr().unwrap());

    let opt = Arc::new(opt);

    while let Some(handshake) = incoming.next().await {
        let opt = opt.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(handshake, opt).await {
                error!("connection lost: {:#}", e);
            }
        });
    }

    Ok(())
}

async fn handle(handshake: quinn::Connecting, opt: Arc<Opt>) -> Result<()> {
    let quinn::NewConnection {
        uni_streams,
        bi_streams,
        connection,
        ..
    } = handshake.await.context("handshake failed")?;
    debug!("{} connected", connection.remote_address());
    tokio::try_join!(
        drive_uni(connection.clone(), uni_streams),
        drive_bi(bi_streams),
        conn_stats(connection, opt)
    )?;
    Ok(())
}

async fn conn_stats(connection: quinn::Connection, opt: Arc<Opt>) -> Result<()> {
    if opt.conn_stats {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            println!("{:?}\n", connection.stats());
        }
    }

    Ok(())
}

async fn drive_uni(
    connection: quinn::Connection,
    mut streams: quinn::IncomingUniStreams,
) -> Result<()> {
    while let Some(stream) = streams.next().await {
        let stream = stream?;
        let connection = connection.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_uni(connection, stream).await {
                error!("request failed: {:#}", e);
            }
        });
    }
    Ok(())
}

async fn handle_uni(connection: quinn::Connection, stream: quinn::RecvStream) -> Result<()> {
    let bytes = read_req(stream).await?;
    let response = connection.open_uni().await?;
    respond(bytes, response).await?;
    Ok(())
}

async fn drive_bi(mut streams: quinn::IncomingBiStreams) -> Result<()> {
    while let Some(stream) = streams.next().await {
        let (send, recv) = stream?;
        tokio::spawn(async move {
            if let Err(e) = handle_bi(send, recv).await {
                error!("request failed: {:#}", e);
            }
        });
    }
    Ok(())
}

async fn handle_bi(send: quinn::SendStream, recv: quinn::RecvStream) -> Result<()> {
    let bytes = read_req(recv).await?;
    respond(bytes, send).await?;
    Ok(())
}

async fn read_req(mut stream: quinn::RecvStream) -> Result<u64> {
    let mut buf = [0; 8];
    stream
        .read_exact(&mut buf)
        .await
        .context("reading request")?;
    let n = u64::from_be_bytes(buf);
    debug!("got req for {} bytes on {}", n, stream.id());
    drain_stream(stream).await?;
    Ok(n)
}

async fn drain_stream(mut stream: quinn::RecvStream) -> Result<()> {
    #[rustfmt::skip]
    let mut bufs = [
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
        Bytes::new(), Bytes::new(), Bytes::new(), Bytes::new(),
    ];
    while stream.read_chunks(&mut bufs[..]).await?.is_some() {}
    debug!("finished reading {}", stream.id());
    Ok(())
}

async fn respond(mut bytes: u64, mut stream: quinn::SendStream) -> Result<()> {
    const DATA: [u8; 1024 * 1024] = [42; 1024 * 1024];

    while bytes > 0 {
        let chunk_len = bytes.min(DATA.len() as u64);
        stream
            .write_chunk(Bytes::from_static(&DATA[..chunk_len as usize]))
            .await
            .context("sending response")?;
        bytes -= chunk_len;
    }
    debug!("finished responding on {}", stream.id());
    Ok(())
}

static PERF_CIPHER_SUITES: &[rustls::SupportedCipherSuite] = &[
    rustls::cipher_suite::TLS13_AES_128_GCM_SHA256,
    rustls::cipher_suite::TLS13_AES_256_GCM_SHA384,
    rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
];
