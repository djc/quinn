use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket},
    sync::Arc,
    thread,
};

use bencher::{benchmark_group, benchmark_main, Bencher};
use futures::StreamExt;
use tokio::runtime::{Builder, Runtime};
use tracing::error_span;
use tracing_futures::Instrument as _;

use quinn::Endpoint;

benchmark_group!(
    benches,
    large_data_1_stream,
    large_data_10_streams,
    small_data_1_stream,
    small_data_100_streams
);
benchmark_main!(benches);

fn large_data_1_stream(bench: &mut Bencher) {
    send_data(bench, LARGE_DATA, 1);
}

fn large_data_10_streams(bench: &mut Bencher) {
    send_data(bench, LARGE_DATA, 10);
}

fn small_data_1_stream(bench: &mut Bencher) {
    send_data(bench, SMALL_DATA, 1);
}

fn small_data_100_streams(bench: &mut Bencher) {
    send_data(bench, SMALL_DATA, 100);
}

fn send_data(bench: &mut Bencher, data: &'static [u8], concurrent_streams: usize) {
    let _ = tracing_subscriber::fmt::try_init();

    let ctx = Context::new();
    let (addr, thread) = ctx.spawn_server();
    let (endpoint, client, runtime) = ctx.make_client(addr);
    let client = Arc::new(client);

    bench.bytes = (data.len() as u64) * (concurrent_streams as u64);
    bench.iter(|| {
        let mut handles = Vec::new();

        for _ in 0..concurrent_streams {
            let client = client.clone();
            handles.push(runtime.spawn(async move {
                let mut stream = client.open_uni().await.unwrap();
                stream.write_all(data).await.unwrap();
                stream.finish().await.unwrap();
            }));
        }

        runtime.block_on(async {
            for handle in handles {
                handle.await.unwrap();
            }
        });
    });
    drop(client);
    runtime.block_on(endpoint.wait_idle());
    thread.join().unwrap()
}

struct Context {
    server_config: quinn::ServerConfig,
    client_config: quinn::ClientConfig,
}

impl Context {
    #[allow(clippy::field_reassign_with_default)] // https://github.com/rust-lang/rust-clippy/issues/6527
    fn new() -> Self {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = quinn::PrivateKey::from_der(&cert.serialize_private_key_der()).unwrap();
        let cert = quinn::Certificate::from_der(&cert.serialize_der().unwrap()).unwrap();
        let cert_chain = quinn::CertificateChain::from_certs(vec![cert.clone()]);

        let mut server_config = quinn::ServerConfig::with_single_cert(cert_chain, key).unwrap();
        Arc::get_mut(&mut server_config.transport)
            .unwrap()
            .max_concurrent_uni_streams(1024)
            .unwrap();

        Self {
            server_config,
            client_config: quinn::ClientConfig::with_root_certificates(vec![cert], None).unwrap(),
        }
    }

    pub fn spawn_server(&self) -> (SocketAddr, thread::JoinHandle<()>) {
        let sock = UdpSocket::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0)).unwrap();
        let addr = sock.local_addr().unwrap();
        let config = self.server_config.clone();
        let handle = thread::spawn(move || {
            let mut endpoint = Endpoint::builder();
            endpoint.listen(config);
            let runtime = rt();
            let (_, mut incoming) = {
                let _guard = runtime.enter();
                endpoint.with_socket(sock).unwrap()
            };
            let handle = runtime.spawn(
                async move {
                    let quinn::NewConnection {
                        mut uni_streams, ..
                    } = incoming
                        .next()
                        .await
                        .expect("accept")
                        .await
                        .expect("connect");

                    while let Some(Ok(mut stream)) = uni_streams.next().await {
                        tokio::spawn(async move {
                            while stream
                                .read_chunk(usize::MAX, false)
                                .await
                                .unwrap()
                                .is_some()
                            {}
                        });
                    }
                }
                .instrument(error_span!("server")),
            );
            runtime.block_on(handle).unwrap();
        });
        (addr, handle)
    }

    pub fn make_client(
        &self,
        server_addr: SocketAddr,
    ) -> (quinn::Endpoint, quinn::Connection, Runtime) {
        let runtime = rt();
        let (endpoint, _) = {
            let _guard = runtime.enter();
            Endpoint::builder()
                .bind(&SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0))
                .unwrap()
        };
        let quinn::NewConnection { connection, .. } = runtime
            .block_on(async {
                endpoint
                    .connect_with(self.client_config.clone(), &server_addr, "localhost")
                    .unwrap()
                    .instrument(error_span!("client"))
                    .await
            })
            .unwrap();
        (endpoint, connection, runtime)
    }
}

fn rt() -> Runtime {
    Builder::new_current_thread().enable_all().build().unwrap()
}

const LARGE_DATA: &[u8] = &[0xAB; 1024 * 1024];

const SMALL_DATA: &[u8] = &[0xAB; 1];
