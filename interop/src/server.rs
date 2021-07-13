#![type_length_limit = "2121396"]

use std::{
    ascii, cmp,
    ffi::OsStr,
    fs,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    pin::Pin,
    str, sync,
    task::{Context, Poll},
};

use anyhow::{anyhow, bail, Context as _, Result};
use bytes::Bytes;
use futures::{ready, Future, StreamExt, TryFutureExt};
use http::{Response, StatusCode};
use hyper::service::{make_service_fn, service_fn};
use structopt::{self, StructOpt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tracing::{error, info, info_span};
use tracing_futures::Instrument as _;

use quinn::SendStream;
use sync::Arc;

#[derive(StructOpt, Debug, Clone)]
#[structopt(name = "h3_server")]
struct Opt {
    /// TLS private key
    #[structopt(
        parse(from_os_str),
        short = "k",
        long = "key",
        requires = "cert",
        default_value = "key.der"
    )]
    key: PathBuf,
    /// TLS certificate
    #[structopt(
        parse(from_os_str),
        short = "c",
        long = "cert",
        requires = "key",
        default_value = "cert.der"
    )]
    cert: PathBuf,
    /// Address to listen on
    #[structopt(long = "listen", short = "l", default_value = "::")]
    listen: IpAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();
    let opt = Opt::from_args();

    let key = fs::read(&opt.key).context("failed to read private key")?;
    let key = match opt.key.as_path().extension().and_then(OsStr::to_str) {
        Some("der") => quinn::PrivateKey::from_der(&key[..])?,
        _ => quinn::PrivateKey::from_pem(&key[..])?,
    };

    let cert_chain = fs::read(&opt.cert).context("failed to read certificate chain")?;
    let cert_chain = match opt.cert.as_path().extension().and_then(OsStr::to_str) {
        Some("der") => {
            quinn::CertificateChain::from_certs(vec![quinn::Certificate::from_der(&cert_chain)?])
        }
        _ => quinn::CertificateChain::from_pem(&cert_chain)?,
    };

    let mut server_config = quinn::ServerConfig::with_single_cert(cert_chain, key)?;
    Arc::get_mut(&mut server_config.crypto)
        .unwrap()
        .alpn_protocols = vec![b"hq-29".to_vec(), b"siduck-00".to_vec()];

    let main = server(server_config.clone(), SocketAddr::new(opt.listen, 4433));
    let default = server(server_config.clone(), SocketAddr::new(opt.listen, 443));
    server_config.use_stateless_retry(true);
    let retry = server(server_config.clone(), SocketAddr::new(opt.listen, 4434));

    tokio::try_join!(main, default, retry, h2_server(server_config))?;

    Ok(())
}

async fn server(mut server_config: quinn::ServerConfig, addr: SocketAddr) -> Result<()> {
    let mut transport = quinn::TransportConfig::default();
    transport.send_window(1024 * 1024 * 3);
    transport.receive_window(1024 * 1024).unwrap();
    server_config.transport = Arc::new(transport);

    let mut endpoint_builder = quinn::Endpoint::builder();
    endpoint_builder.listen(server_config);
    let (_, mut incoming) = endpoint_builder.bind(&addr)?;

    println!("server listening on {}", addr);
    while let Some(mut connecting) = incoming.next().await {
        tokio::spawn(async move {
            let proto = match connecting.handshake_data().await {
                Err(_) => return,
                Ok(x) => x.protocol.unwrap(),
            };
            println!("server received connection");

            let result = match &proto[..] {
                b"hq-29" => hq_handle_connection(connecting).await,
                b"siduck-00" => siduck_handle_connection(connecting).await,
                _ => unreachable!("unsupported protocol"),
            };
            if let Err(e) = result {
                error!("handling connection failed: {:?}", e);
            }
        });
    }
    Ok(())
}

async fn hq_handle_connection(conn: quinn::Connecting) -> Result<()> {
    let quinn::NewConnection {
        connection,
        mut bi_streams,
        ..
    } = match conn.into_0rtt() {
        Ok((c, _)) => c,
        Err(c) => c.await?,
    };
    let span = info_span!(
        "connection",
        remote = %connection.remote_address(),
        protocol = %connection
            .handshake_data()
            .unwrap()
            .protocol
            .map_or_else(|| "<none>".into(), |x| String::from_utf8_lossy(&x).into_owned())
    );
    async {
        info!("established");

        // Each stream initiated by the client constitutes a new request.
        while let Some(stream) = bi_streams.next().await {
            let stream = match stream {
                Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                    info!("connection closed");
                    return Ok(());
                }
                Err(e) => {
                    return Err(e);
                }
                Ok(s) => s,
            };
            tokio::spawn(
                hq_handle_request(stream)
                    .unwrap_or_else(move |e| error!("failed: {reason}", reason = e.to_string()))
                    .instrument(info_span!("request")),
            );
        }
        Ok(())
    }
    .instrument(span)
    .await?;
    Ok(())
}

async fn hq_handle_request((send, recv): (quinn::SendStream, quinn::RecvStream)) -> Result<()> {
    let req = recv
        .read_to_end(64 * 1024)
        .await
        .map_err(|e| anyhow!("failed reading request: {}", e))?;
    let mut escaped = String::new();
    for &x in &req[..] {
        let part = ascii::escape_default(x).collect::<Vec<_>>();
        escaped.push_str(str::from_utf8(&part).unwrap());
    }
    info!(content = %escaped);
    // Execute the request
    hq_process_get(send, &req).await?;
    Ok(())
}

async fn hq_process_get(mut send: SendStream, x: &[u8]) -> Result<()> {
    if x.len() < 4 || &x[0..4] != b"GET " {
        bail!("missing GET");
    }
    if x[4..].len() < 2 || &x[x.len() - 2..] != b"\r\n" {
        bail!("missing \\r\\n");
    }
    let x = &x[4..x.len() - 2];
    let end = x.iter().position(|&c| c == b' ').unwrap_or_else(|| x.len());
    let path = str::from_utf8(&x[..end]).context("path is malformed UTF-8")?;

    // Write the response
    match parse_size(path) {
        Ok(n) if n <= 1_000_000_000 => {
            let mut remaining = n;
            while remaining > 0 {
                let size = cmp::min(remaining, TEXT.len());
                send.write_all(&TEXT[..size])
                    .await
                    .map_err(|e| anyhow!("failed to send response: {}", e))?;
                remaining -= size;
            }
        }
        Ok(_) | Err(_) => {
            send.write_all(&HOME.as_bytes())
                .await
                .map_err(|e| anyhow!("failed to send response: {}", e))?;
        }
    }

    // Gracefully terminate the stream
    send.finish()
        .await
        .map_err(|e| anyhow!("failed to shutdown stream: {}", e))?;

    Ok(())
}

fn parse_size(literal: &str) -> Result<usize> {
    if literal.is_empty() {
        return Err(anyhow!("path empty"));
    }
    let pos = literal[1..]
        .find(|c: char| !c.is_ascii_digit())
        .map(|p| p + 1)
        .unwrap_or_else(|| literal.len());
    let num: usize = literal[1..pos]
        .parse()
        .map_err(|_| anyhow!("parse failed"))?;
    let scale = match literal[pos..].to_uppercase().as_str() {
        "K" => 1000,
        "M" => 1_000_000,
        "G" => 1_000_000_000,
        _ => 1,
    };
    Ok(num * scale)
}

const ALT_SVC: &str = "h3-29=\":443\"";

fn h2_home() -> hyper::Response<hyper::Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header("Alt-Svc", ALT_SVC)
        .body(HOME.into())
        .expect("failed to build response")
}

fn h2_payload(len: usize) -> hyper::Response<hyper::Body> {
    if len > 1_000_000_000 {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header("Alt-Svc", ALT_SVC)
            .body(Bytes::from(format!("requested {}: too large", len)).into())
            .expect("failed to build response");
        return response;
    }

    let mut buf = TEXT.repeat(len / TEXT.len() + 1);
    buf.truncate(len);
    Response::builder()
        .status(StatusCode::OK)
        .body(buf.into())
        .expect("failed to build response")
}

async fn h2_handle(request: hyper::Request<hyper::Body>) -> Result<hyper::Response<hyper::Body>> {
    Ok(match request.uri().path() {
        "/" => h2_home(),
        path => match parse_size(path) {
            Ok(n) => h2_payload(n),
            Err(_) => h2_home(),
        },
    })
}

async fn h2_server(server_config: quinn::ServerConfig) -> Result<()> {
    let mut tls_cfg = (*server_config.crypto).clone();
    tls_cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let tls_acceptor = TlsAcceptor::from(sync::Arc::new(tls_cfg));

    let tcp = TcpListener::bind(&SocketAddr::new([0, 0, 0, 0].into(), 443)).await?;

    let service = make_service_fn(|_conn| async { Ok::<_, anyhow::Error>(service_fn(h2_handle)) });
    let server = hyper::Server::builder(HyperAcceptor::new(tcp, tls_acceptor)).serve(service);

    if let Err(e) = server.await {
        error!("server error: {}", e);
    }
    Ok(())
}

struct HyperAcceptor {
    tcp: TcpListener,
    tls: TlsAcceptor,
    handshake: Option<tokio_rustls::Accept<TcpStream>>,
}

impl HyperAcceptor {
    pub fn new(tcp: TcpListener, tls: TlsAcceptor) -> Self {
        Self {
            tls,
            tcp,
            handshake: None,
        }
    }
}

impl hyper::server::accept::Accept for HyperAcceptor {
    type Conn = TlsStream<TcpStream>;
    type Error = anyhow::Error;

    fn poll_accept(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<Option<Result<Self::Conn, Self::Error>>> {
        loop {
            match self.handshake {
                Some(ref mut h) => {
                    let conn = ready!(Pin::new(h).poll(cx))?;
                    self.handshake = None;
                    return Poll::Ready(Some(Ok(conn)));
                }
                None => {
                    let (stream, _) = ready!(self.tcp.poll_accept(cx))?;
                    self.handshake = Some(self.tls.accept(stream));
                }
            }
        }
    }
}

const TEXT: &[u8] =
    b"It would be different if we could not step back and reflect on the process,\n\
    but were merely led from impulse to impulse without self- consciousness. But human\n\
    beings do not act solely on impulse. They are prudent, they reflect, they weigh\n\
    consequences, they ask whether what they are doing is worth while. Not only are their\n\
    lives full of particular choices that hang together in larger activities with temporal\n\
    structure: they also decide in the broadest terms what to pursue and what to avoid, what\n\
    the priorities among their various aims should be, and what kind of people they want to\n\
    be or become. Some men are faced with such choices by the large decisions they make from\n\
    time to time; some merely by reflection on the course their lives are taking as the product\n\
    of countless small decisions. They decide whom to marry, what profession to follow, whether\n\
    to join the Country Club, or the Resistance; or they may just wonder why they go on being\n\
    salesmen or academics or taxi drivers, and then stop thinking about it after a certain period\n\
    of inconclusive reflection.";

const HOME: &str = r##"
<html xmlns="http://www.w3.org/1999/xhtml" xml:lang="en" lang="en">
  <head>
    <title>Quinn H3 interop server</title>
  </head>
  <body>
    <h1>Welcome to the quinn-h3 interop server.</h1>
    <p>
      <strong>Draft version:</strong> draft-24<br/>
      <strong>Available tests:</strong> VHDCRZSBU3
    </p>
    <p>
      Use '/{n}' to get <i>n</i> bytes of deep thoughts.<br/>
      For example <a href="/1000000">/1000000</a>
      to get 1MB. Limit: 1GB
    </p>
    <p>Checkout our project's <a href="https://github.com/djc/quinn">repository</a>.</p>
    <p>Say hi on quickdev slack workspace at `quinn`.</p>
  </body>
</html>
"##;

/// https://tools.ietf.org/html/draft-pardue-quic-siduck-00
async fn siduck_handle_connection(conn: quinn::Connecting) -> Result<()> {
    let quinn::NewConnection {
        connection,
        mut datagrams,
        ..
    } = match conn.into_0rtt() {
        Ok((c, _)) => c,
        Err(c) => c.await?,
    };
    while let Some(datagram) = datagrams.next().await {
        match datagram {
            Err(quinn::ConnectionError::ApplicationClosed { .. }) => {
                info!("connection closed");
                return Ok(());
            }
            Err(e) => {
                return Err(e.into());
            }
            Ok(data) => {
                if &data[..] == b"quack" {
                    connection.send_datagram(b"quack-ack"[..].into())?;
                } else {
                    const SIDUCK_ONLY_QUACKS_ECHO: quinn::VarInt = quinn::VarInt::from_u32(0x101);
                    connection.close(SIDUCK_ONLY_QUACKS_ECHO, b"quack quack quack");
                    bail!("got non-quack datagram");
                }
            }
        }
    }
    Ok(())
}
