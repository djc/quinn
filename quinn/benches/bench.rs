use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    sync::Arc,
    thread,
};
use winapi::um::winsock2;

use bencher::{benchmark_group, benchmark_main, Bencher};
use futures::StreamExt;
use tokio::runtime::{Builder, Runtime};
use tracing::error_span;
use tracing_futures::Instrument as _;

use quinn::{create_wsa_socket, ClientConfigBuilder, Endpoint, ServerConfigBuilder};

benchmark_group!(benches, large_streams, small_streams);
benchmark_main!(benches);

fn large_streams(bench: &mut Bencher) {
    let _ = tracing_subscriber::fmt::try_init();

    let ctx = Context::new();
    let (addr, thread) = ctx.spawn_server();
    let (endpoint, client, mut runtime) = ctx.make_client(addr);
    const DATA: &[u8] = &[0xAB; 1024 * 1024];
    bench.bytes = DATA.len() as u64;
    bench.iter(|| {
        runtime.block_on(async {
            let mut stream = client.open_uni().await.unwrap();
            stream.write_all(DATA).await.unwrap();
            stream.finish().await.unwrap();
        });
    });
    drop(client);
    runtime.block_on(endpoint.wait_idle());
    thread.join().unwrap();
}

fn small_streams(bench: &mut Bencher) {
    let _ = tracing_subscriber::fmt::try_init();

    let ctx = Context::new();
    let (addr, thread) = ctx.spawn_server();
    let (endpoint, client, mut runtime) = ctx.make_client(addr);
    const DATA: &[u8] = &[0xAB; 1];
    bench.bytes = 1;
    bench.iter(|| {
        runtime.block_on(async {
            let mut stream = client.open_uni().await.unwrap();
            stream.write_all(DATA).await.unwrap();
        });
    });
    drop(client);
    runtime.block_on(endpoint.wait_idle());
    thread.join().unwrap();
}

struct Context {
    server_config: quinn::ServerConfig,
    client_config: quinn::ClientConfig,
}

impl Context {
    fn new() -> Self {
        let mut winsock_data = winsock2::WSADATA::default();
        if unsafe { winsock2::WSAStartup(0x202, &mut winsock_data) } != 0 {
            panic!("Error starting winsock");
        }

        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = quinn::PrivateKey::from_der(&cert.serialize_private_key_der()).unwrap();
        let cert = quinn::Certificate::from_der(&cert.serialize_der().unwrap()).unwrap();
        let cert_chain = quinn::CertificateChain::from_certs(vec![cert.clone()]);

        let mut transport = quinn::TransportConfig::default();
        transport.stream_window_uni(1024).unwrap();
        let mut server_config = quinn::ServerConfig::default();
        server_config.transport = Arc::new(transport);
        let mut server_config = ServerConfigBuilder::new(server_config);
        server_config.certificate(cert_chain, key).unwrap();

        let mut client_config = ClientConfigBuilder::default();
        client_config.add_certificate_authority(cert).unwrap();

        Self {
            server_config: server_config.build(),
            client_config: client_config.build(),
        }
    }

    pub fn spawn_server(&self) -> (SocketAddr, thread::JoinHandle<()>) {
        let sock = create_wsa_socket(&SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).unwrap();
        let addr = sock.local_addr().unwrap();
        println!("Server address: {:?}", addr);

        let config = self.server_config.clone();
        let handle = thread::spawn(move || {
            let mut endpoint = Endpoint::builder();
            endpoint.listen(config);
            let mut runtime = rt();
            let (_, mut incoming) = runtime.enter(|| endpoint.with_socket(sock).unwrap());
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
                        while stream.read_unordered().await.unwrap().is_some() {}
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
        let mut runtime = rt();
        let (endpoint, _) = runtime.enter(|| {
            Endpoint::builder()
                .bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .unwrap()
        });
        let quinn::NewConnection { connection, .. } = runtime
            .block_on(runtime.enter(|| {
                endpoint
                    .connect_with(self.client_config.clone(), &server_addr, "localhost")
                    .unwrap()
                    .instrument(error_span!("client"))
            }))
            .unwrap();
        (endpoint, connection, runtime)
    }
}

fn rt() -> Runtime {
    Builder::new()
        .basic_scheduler()
        .enable_all()
        .build()
        .unwrap()
}
