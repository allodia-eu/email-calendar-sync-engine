//! What every request the shared client sends carries.

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn every_request_names_the_hosts_product() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}/", listener.local_addr().expect("address"));
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|end| end == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).await.expect("read");
            request.extend_from_slice(&buffer[..read]);
        }
        socket
            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            .await
            .expect("write");
        String::from_utf8_lossy(&request).to_lowercase()
    });

    let agent = crate::UserAgent::new("example-mail", "1.2.3").unwrap();
    crate::client(&engine_tls::TlsClientConfig::bundled(), &agent)
        .build()
        .expect("client")
        .get(&url)
        .send()
        .await
        .expect("send");

    let request = server.await.expect("server");
    assert!(
        request.contains("\r\nuser-agent: example-mail/1.2.3\r\n"),
        "{request}"
    );
}
