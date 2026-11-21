use std::sync::Arc;

fn main() {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let name = "example.com".try_into().unwrap();
    let mut conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let mut hello = Vec::new();
    conn.write_tls(&mut hello).unwrap();
    println!("client hello written: {}", hello.len() > 100);
    println!("aws-lc-rs suites: {}", rustls::crypto::aws_lc_rs::default_provider().cipher_suites.len());
}
