# Certificates

In this chapter, we discuss the configuration of the certificates that are **required** for a working Quinn connection. 

As QUIC uses TLS 1.3 for authentication of connections, the server needs to provide the client with a certificate confirming its identity, and the client must be configured to trust the certificates it receives from the server. 

## Insecure Connection

For our example use case, the easiest way to allow the client to trust our server is to disable certificate verification (don't do this in production!). 
When the [rustls][3] `dangerous_configuration` feature flag is enabled, a client can be configured to trust any server.

Start by adding a [rustls][3] dependency with the `dangerous_configuration` feature flag to your `Cargo.toml` file.

```toml
quinn = "*"
rustls = { version = "*", features = ["dangerous_configuration", "quic"] }
``` 

Then, allow the client to skip the certificate validation by implementing [ServerCertVerifier][ServerCertVerifier] and letting it assert verification for any server. 

```rust
// Implementation of `ServerCertVerifier` that verifies everything as trustworthy.
struct SkipCertificationVerification;

impl rustls::ServerCertVerifier for SkipCertificationVerification {
    fn verify_server_cert(
        &self, _: &rustls::RootCertStore, _: &[rustls::Certificate], _: webpki::DNSNameRef, _: &[u8],
    ) -> Result<rustls::ServerCertVerified, rustls::TLSError> {
        Ok(ServerCertVerified::assertion())
    }
}
```

After that, modify the [ClientConfig][ClientConfig] to use this [ServerCertVerifier][ServerCertVerifier] implementation. 

```rust
pub fn insecure() -> ClientConfig {
    let mut cfg = quinn::ClientConfigBuilder::default().build();

    // Get a mutable reference to the 'crypto' config in the 'client config'.
    let tls_cfg: &mut rustls::ClientConfig =
        std::sync::Arc::get_mut(&mut cfg.crypto).unwrap();

    // Change the certification verifier.
    // This is only available when compiled with the 'dangerous_configuration' feature.
    tls_cfg
        .dangerous()
        .set_certificate_verifier(Arc::new(SkipCertificationVerification));
    cfg
}
```
 
Finally, if you plug this [ClientConfig][ClientConfig] into the [EndpointBuilder::default_client_config()][default_client_config] your client endpoint should verify all connections as trustworthy.

## Using Certificates

In this section, we look at certifying an endpoint with a certificate. 
The certificate can be signed with its key, or with a certificate authority's key.

### Self Signed Certificates

Relying on [self-signed][5] certificates means that clients allow servers to sign their certificates. 
This is simpler because no third party is involved in signing the server's certificate.
However, self-signed certificates do not protect users from person-in-the-middle attacks, because an interceptor can trivially replace the certificate with one that it has signed. Self-signed certificates, among other options, can be created using the [rcgen][4] crate or the openssl binary.
This example uses [rcgen][4] to generate a certificate.

Let's look at an example:

```rust
pub fn generate_self_signed_cert(cert_path: &str, key_path: &str) -> anyhow::Result<(quinn::Certificate, quinn::PrivateKey)> {
    // Generate dummy certificate.
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let serialized_key = certificate.serialize_private_key_der();
    let serialized_certificate = certificate.serialize_der().unwrap();

    // Write to files.
    fs::write(&cert_path, &serialized_certificate).context("failed to write certificate")?;
    fs::write(&key_path, &serialized_key).context("failed to write private key")?;

    let cert = quinn::Certificate::from_der(&serialized_certificate)?;
    let key = quinn::PrivateKey::from_der(&serialized_key)?;
    Ok((cert, key))
}
```

*Note that [generate_simple_self_signed][generate_simple_self_signed] returns a [Certificate][2] that can be serialized to both `.der` and `.pem` formats.*

### Non-self-signed Certificates

For this example, we use [Let's Encrypt][6], a well-known Certificate Authority ([CA][1]) (certificate issuer) which distributes certificates for free.

**Generate Certificate**

[certbot][7] can be used with Let's Encrypt to generate certificates; its website comes with clear instructions.
Because we're generating a certificate for an internal test server, the process used will be slightly different compared to what you would do when generating certificates for an existing (public) website.

On the certbot website, select that you do not have a public web server and follow the given installation instructions.
certbot must answer a cryptographic challenge of the Let's Encrypt API to prove that you control the domain. 
It needs to listen on port 80 (HTTP) or 443 (HTTPS) to achieve this. Open the appropriate port in your firewall and router.

If certbot is installed, run `certbot certonly --standalone`, this command will start a web server in the background and start the challenge.
certbot asks for the required data and writes the certificate to `cert.pem` and the private key to `privkey.pem`.  
These files can then be referenced in code.  
 
```rust
pub fn read_cert_from_file() -> anyhow::Result<(quinn::Certificate, quinn::PrivateKey)> {
    // Read from certificate and key from directory.
    let (cert, key) = fs::read(&"./cert.pem").and_then(|x| Ok((x, fs::read(&"./privkey.pem")?)))?;

    // Parse to certificate chain whereafter taking the first certifcater in this chain.
    let cert = quinn::CertificateChain::from_pem(&cert)?.iter().next().unwrap().clone();
    let key = quinn::PrivateKey::from_pem(&key)?;

    Ok((quinn::Certificate::from(cert), key))
}
```

### Configuring Certificates

Now that you have a valid certificate, the client and server need to be configured to use it.
After configuring plug the configuration into the `Endpoint`.

**Configure Server**

```rust
let mut builder = ServerConfigBuilder::default();
builder.certificate(CertificateChain::from_certs(vec![certificate]), key)?;
```

This is the only thing you need to do for your server to be secured. 

**Configure Client**

```rust
let mut builder = ClientConfigBuilder::default();
builder.add_certificate_authority(certificate)?;    
```

This is the only thing you need to do for your client to trust a server certificate. 

<br><hr>

[Next](set-up-connection.md), let's have a look at how to set up a connection. 

[1]: https://en.wikipedia.org/wiki/Certificate_authority
[2]: https://en.wikipedia.org/wiki/Public_key_certificate
[3]: https://github.com/ctz/rustls
[4]: https://github.com/est31/rcgen
[5]: https://en.wikipedia.org/wiki/Self-signed_certificate#:~:text=In%20cryptography%20and%20computer%20security,a%20CA%20aim%20to%20provide.
[6]: https://letsencrypt.org/getting-started/
[7]: https://certbot.eff.org/instructions

[ClientConfig]: https://docs.rs/quinn/latest/quinn/generic/struct.ClientConfig.html
[ServerCertVerifier]: https://docs.rs/rustls/latest/rustls/trait.ServerCertVerifier.html
[default_client_config]: https://docs.rs/quinn/latest/quinn/generic/struct.EndpointBuilder.html#method.default_client_config
[generate_simple_self_signed]: https://docs.rs/rcgen/latest/rcgen/fn.generate_simple_self_signed.html
[Certificate]: https://docs.rs/rcgen/latest/rcgen/struct.Certificate.html
