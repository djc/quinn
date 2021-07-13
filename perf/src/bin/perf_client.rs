use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures::StreamExt;
use hdrhistogram::Histogram;
use rustls::SupportedCipherSuite;
use structopt::StructOpt;
use tokio::sync::Semaphore;
use tracing::{debug, error, info};

use perf::bind_socket;

/// Connects to a QUIC perf server and maintains a specified pattern of requests until interrupted
#[derive(StructOpt)]
#[structopt(name = "client")]
struct Opt {
    /// Host to connect to
    #[structopt(default_value = "localhost:4433")]
    host: String,
    /// Override DNS resolution for host
    #[structopt(long)]
    ip: Option<IpAddr>,
    /// Number of unidirectional requests to maintain concurrently
    #[structopt(long, default_value = "0")]
    uni_requests: u64,
    /// Number of bidirectional requests to maintain concurrently
    #[structopt(long, default_value = "1")]
    bi_requests: u64,
    /// Number of bytes to request
    #[structopt(long, default_value = "1048576")]
    download_size: u64,
    /// Number of bytes to transmit, in addition to the request header
    #[structopt(long, default_value = "1048576")]
    upload_size: u64,
    /// The time to run in seconds
    #[structopt(long, default_value = "60")]
    duration: u64,
    /// Send buffer size in bytes
    #[structopt(long, default_value = "2097152")]
    send_buffer_size: usize,
    /// Receive buffer size in bytes
    #[structopt(long, default_value = "2097152")]
    recv_buffer_size: usize,
    /// Specify the local socket address
    #[structopt(long)]
    local_addr: Option<SocketAddr>,
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
    let mut host_parts = opt.host.split(':');
    let host_name = host_parts.next().unwrap();
    let host_port = host_parts
        .next()
        .map_or(Ok(443), |x| x.parse())
        .context("parsing port")?;
    let addr = match opt.ip {
        None => tokio::net::lookup_host(&opt.host)
            .await
            .context("resolving host")?
            .next()
            .unwrap(),
        Some(ip) => SocketAddr::new(ip, host_port),
    };

    info!("connecting to {} at {}", host_name, addr);

    let bind_addr = opt.local_addr.unwrap_or_else(|| {
        let unspec = if addr.is_ipv4() {
            Ipv4Addr::UNSPECIFIED.into()
        } else {
            Ipv6Addr::UNSPECIFIED.into()
        };
        SocketAddr::new(unspec, 0)
    });

    info!("local addr {:?}", bind_addr);

    let socket = bind_socket(bind_addr, opt.send_buffer_size, opt.recv_buffer_size)?;

    let endpoint = quinn::EndpointBuilder::default();

    let (endpoint, _) = endpoint.with_socket(socket).context("binding endpoint")?;

    let mut crypto = rustls::ClientConfig::builder()
        .with_cipher_suites(PERF_CIPHER_SUITES)
        .with_safe_default_kx_groups()
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"perf".to_vec()];

    let transport = Arc::new(quinn::TransportConfig::default());
    let cfg = quinn::ClientConfig {
        crypto: Arc::new(crypto),
        transport,
    };

    let stats = Arc::new(Mutex::new(Stats::default()));

    let quinn::NewConnection {
        connection,
        uni_streams,
        ..
    } = endpoint
        .connect_with(cfg, &addr, &host_name)?
        .await
        .context("connecting")?;

    info!("established");

    let acceptor = UniAcceptor(Arc::new(tokio::sync::Mutex::new(uni_streams)));

    let drive_fut = async {
        tokio::try_join!(
            drive_uni(
                connection.clone(),
                acceptor,
                stats.clone(),
                opt.uni_requests,
                opt.upload_size,
                opt.download_size
            ),
            drive_bi(
                connection.clone(),
                stats.clone(),
                opt.bi_requests,
                opt.upload_size,
                opt.download_size
            )
        )
    };

    let print_fut = async {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            {
                let guard = stats.lock().unwrap();
                guard.print();
                if opt.conn_stats {
                    println!("{:?}\n", connection.stats());
                }
            }
        }
    };

    tokio::select! {
        _ = drive_fut => {}
        _ = print_fut => {}
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
            connection.close(0u32.into(), b"interrupted");
        }
        _ = tokio::time::sleep(Duration::from_secs(opt.duration)) => {
            info!("shutting down");
            connection.close(0u32.into(), b"done");
        }
    }

    endpoint.wait_idle().await;

    // TODO: Print stats

    Ok(())
}

async fn drain_stream(mut stream: quinn::RecvStream, stats: &mut RequestStats) -> Result<()> {
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
    while stream.read_chunks(&mut bufs[..]).await?.is_some() {
        if stats.first_byte.is_none() {
            stats.first_byte = Some(Instant::now());
        }
    }

    let now = Instant::now();
    if stats.first_byte.is_none() {
        stats.first_byte = Some(now);
    }
    stats.download_end = Some(now);

    debug!("response finished on {}", stream.id());
    Ok(())
}

async fn drive_uni(
    connection: quinn::Connection,
    acceptor: UniAcceptor,
    stats: Arc<Mutex<Stats>>,
    concurrency: u64,
    upload: u64,
    download: u64,
) -> Result<()> {
    let sem = Arc::new(Semaphore::new(concurrency as usize));

    loop {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let mut request_stats = RequestStats::new(upload, download);
        let send = connection.open_uni().await?;
        let acceptor = acceptor.clone();
        let stats = stats.clone();

        debug!("sending request on {}", send.id());
        tokio::spawn(async move {
            if let Err(e) = request_uni(send, acceptor, upload, download, &mut request_stats).await
            {
                error!("sending request failed: {:#}", e);
            } else {
                request_stats.success = true;
            }

            {
                let mut guard = stats.lock().unwrap();
                guard.record(request_stats);
            }

            drop(permit);
        });
    }
}

async fn request_uni(
    send: quinn::SendStream,
    acceptor: UniAcceptor,
    upload: u64,
    download: u64,
    stats: &mut RequestStats,
) -> Result<()> {
    request(send, upload, download, stats).await?;
    let recv = {
        let mut guard = acceptor.0.lock().await;
        guard
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("End of stream"))
    }??;
    drain_stream(recv, stats).await?;
    Ok(())
}

async fn request(
    mut send: quinn::SendStream,
    mut upload: u64,
    download: u64,
    stats: &mut RequestStats,
) -> Result<()> {
    stats.upload_start = Some(Instant::now());
    send.write_all(&download.to_be_bytes()).await?;

    const DATA: [u8; 1024 * 1024] = [42; 1024 * 1024];
    while upload > 0 {
        let chunk_len = upload.min(DATA.len() as u64);
        send.write_chunk(Bytes::from_static(&DATA[..chunk_len as usize]))
            .await
            .context("sending response")?;
        upload -= chunk_len;
    }
    send.finish().await?;

    let now = Instant::now();
    stats.download_start = Some(now);

    debug!("upload finished on {}", send.id());
    Ok(())
}

async fn drive_bi(
    connection: quinn::Connection,
    stats: Arc<Mutex<Stats>>,
    concurrency: u64,
    upload: u64,
    download: u64,
) -> Result<()> {
    let sem = Arc::new(Semaphore::new(concurrency as usize));

    loop {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let mut request_stats = RequestStats::new(upload, download);
        let (send, recv) = connection.open_bi().await?;
        let stats = stats.clone();

        debug!("sending request on {}", send.id());
        tokio::spawn(async move {
            if let Err(e) = request_bi(send, recv, upload, download, &mut request_stats).await {
                error!("request failed: {:#}", e);
            } else {
                request_stats.success = true;
            }

            {
                let mut guard = stats.lock().unwrap();
                guard.record(request_stats);
            }

            drop(permit);
        });
    }
}

async fn request_bi(
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    upload: u64,
    download: u64,
    stats: &mut RequestStats,
) -> Result<()> {
    request(send, upload, download, stats).await?;
    drain_stream(recv, stats).await?;
    Ok(())
}

#[derive(Clone)]
struct UniAcceptor(Arc<tokio::sync::Mutex<quinn::IncomingUniStreams>>);

struct SkipServerVerification;

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::ServerCertVerified, rustls::Error> {
        Ok(rustls::ServerCertVerified::assertion())
    }
}

struct RequestStats {
    start: Instant,
    upload_start: Option<Instant>,
    download_start: Option<Instant>,
    first_byte: Option<Instant>,
    download_end: Option<Instant>,
    upload_size: u64,
    download_size: u64,
    success: bool,
}

impl RequestStats {
    pub fn new(upload_size: u64, download_size: u64) -> Self {
        Self {
            start: Instant::now(),
            upload_start: None,
            download_start: None,
            first_byte: None,
            upload_size,
            download_size,
            download_end: None,
            success: false,
        }
    }
}

struct Stats {
    /// Test start time
    start: Instant,
    /// Durations of complete requests
    duration: Histogram<u64>,
    /// Time from finishing the upload until receiving the first byte of the response
    fbl: Histogram<u64>,
    /// Throughput for uploads
    upload_throughput: Histogram<u64>,
    /// Throughput for downloads
    download_throughput: Histogram<u64>,
    /// The total amount of requests executed
    requests: usize,
    /// The amount of successful requests
    success: usize,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            duration: Histogram::new(3).unwrap(),
            fbl: Histogram::new(3).unwrap(),
            upload_throughput: Histogram::new(3).unwrap(),
            download_throughput: Histogram::new(3).unwrap(),
            requests: 0,
            success: 0,
        }
    }
}

impl Stats {
    pub fn record(&mut self, request: RequestStats) {
        self.requests += 1;
        self.success += if request.success { 1 } else { 0 };

        // Record the remaining metrics only if the request is successful
        // In this case all timings are available
        if !request.success {
            return;
        }

        let duration = request.download_end.unwrap().duration_since(request.start);
        self.duration.record(duration.as_millis() as u64).unwrap();

        let fbl = request
            .first_byte
            .unwrap()
            .duration_since(request.download_start.unwrap());
        self.fbl.record(fbl.as_millis() as u64).unwrap();

        let download_duration = request
            .download_end
            .unwrap()
            .duration_since(request.download_start.unwrap());
        let download_bps = throughput_bps(download_duration, request.download_size);
        self.download_throughput
            .record(download_bps as u64)
            .unwrap();

        let upload_duration = request
            .download_start
            .unwrap()
            .duration_since(request.upload_start.unwrap());
        let upload_bps = throughput_bps(upload_duration, request.upload_size);
        self.upload_throughput.record(upload_bps as u64).unwrap();
    }

    pub fn print(&self) {
        let dt = self.start.elapsed();
        let rps = self.requests as f64 / dt.as_secs_f64();

        println!("Overall stats:");
        println!(
            "RPS: {:.2} ({} requests in {:4.2?})",
            rps, self.requests, dt,
        );
        println!(
            "Success rate: {:4.2}%",
            100.0 * self.success as f64 / self.requests as f64,
        );
        println!();

        println!("Stream metrics:\n");

        println!("      │ Duration  │ FBL       | Upload Throughput | Download Throughput");
        println!("──────┼───────────┼───────────┼───────────────────┼────────────────────");

        let print_metric = |label: &'static str, get_metric: fn(&Histogram<u64>) -> u64| {
            println!(
                " {} │ {:>9} │ {:>9} │ {:11.2} MiB/s │ {:13.2} MiB/s",
                label,
                format!("{:.2?}", Duration::from_millis(get_metric(&self.duration))),
                format!("{:.2?}", Duration::from_millis(get_metric(&self.fbl))),
                get_metric(&self.upload_throughput) as f64 / 1024.0 / 1024.0,
                get_metric(&self.download_throughput) as f64 / 1024.0 / 1024.0,
            );
        };

        print_metric("AVG ", |hist| hist.mean() as u64);
        print_metric("P0  ", |hist| hist.value_at_quantile(0.00));
        print_metric("P10 ", |hist| hist.value_at_quantile(0.10));
        print_metric("P50 ", |hist| hist.value_at_quantile(0.50));
        print_metric("P90 ", |hist| hist.value_at_quantile(0.90));
        print_metric("P100", |hist| hist.value_at_quantile(1.00));
        println!();
    }
}

fn throughput_bps(duration: Duration, size: u64) -> f64 {
    (size as f64) / (duration.as_secs_f64())
}

static PERF_CIPHER_SUITES: &[SupportedCipherSuite] = &[
    rustls::cipher_suite::TLS13_AES_128_GCM_SHA256,
    rustls::cipher_suite::TLS13_AES_256_GCM_SHA384,
    rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
];
