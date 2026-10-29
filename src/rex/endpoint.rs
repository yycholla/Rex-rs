//! Explicit Rex origins and owned Unix/TCP/TLS streams. Authentication stays with Rex.
use anyhow::{Context, Result, bail, ensure};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned, pki_types::ServerName};
use std::{
    io::{self, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, OnceLock, mpsc},
    thread,
    time::{Duration, Instant},
};
use url::{Host, Url};

/// A configured origin, never a credential-bearing URL or arbitrary API path.
#[derive(Clone, Debug)]
pub enum Endpoint {
    Unix(PathBuf),
    Network(Url),
}

impl Endpoint {
    /// Parse a Unix address or HTTP(S)/WS(S) origin without connecting.
    ///
    /// # Errors
    /// Rejects credentials, URL suffixes, unsupported schemes and relative paths.
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(value.len() <= 4096, "Rex endpoint exceeds 4096 bytes");
        ensure!(
            value.trim() == value,
            "Rex endpoint must not have surrounding whitespace"
        );
        ensure!(
            !value.chars().any(char::is_control),
            "invalid Rex endpoint characters"
        );
        if let Some(path) = value.strip_prefix("unix://") {
            let path = PathBuf::from(path);
            ensure!(path.is_absolute(), "Rex socket path must be absolute");
            return Ok(Self::Unix(path));
        }
        let mut url = Url::parse(value).context("invalid Rex endpoint URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https" | "ws" | "wss"),
            "Rex endpoint requires unix://, http://, https://, ws:// or wss://"
        );
        ensure!(url.host().is_some(), "Rex endpoint requires a host");
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            "Rex endpoint must not contain credentials; access is authorized by the server's transport identity"
        );
        ensure!(
            url.query().is_none() && url.fragment().is_none(),
            "Rex endpoint must not contain a query or fragment"
        );
        ensure!(
            matches!(url.path(), "" | "/"),
            "Rex endpoint must be an origin without an API path"
        );
        let scheme = if matches!(url.scheme(), "https" | "wss") {
            "wss"
        } else {
            "ws"
        };
        url.set_scheme(scheme)
            .map_err(|()| anyhow::anyhow!("invalid Rex endpoint scheme"))?;
        Ok(Self::Network(url))
    }

    /// Build the fixed protocol URL without forwarding arbitrary URL suffixes.
    ///
    /// # Errors
    /// Rejects paths outside the Rex control and data APIs.
    pub fn websocket_url(&self, path: &str) -> Result<String> {
        ensure!(
            matches!(path, "/api/control" | "/api/data"),
            "invalid Rex WebSocket path"
        );
        match self {
            Self::Unix(_) => Ok(format!("ws://rex{path}")),
            Self::Network(url) => {
                let mut url = url.clone();
                url.set_path(path);
                Ok(url.to_string())
            }
        }
    }

    /// Connect with bounded DNS, socket and TLS handshakes and system CA roots.
    ///
    /// # Errors
    /// Reports unavailable endpoints, certificate/hostname failures and timeouts.
    pub fn connect(&self, timeout: Duration) -> Result<RexStream> {
        let deadline = Instant::now() + timeout;
        let stream = match self {
            Self::Unix(path) => {
                RexStream::Unix(UnixStream::connect(path).context("connect to Rex Unix socket")?)
            }
            Self::Network(url) => {
                let port = url
                    .port_or_known_default()
                    .context("Rex endpoint lacks port")?;
                let host = url.host().context("Rex endpoint lacks host")?;
                let addresses = match host {
                    Host::Ipv4(ip) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
                    Host::Ipv6(ip) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
                    Host::Domain(name) => resolve(name, port, deadline)?,
                };
                let tcp = connect_tcp(addresses, deadline)?;
                tcp.set_nodelay(true)?;
                tcp.set_read_timeout(Some(remaining(deadline)?))?;
                tcp.set_write_timeout(Some(remaining(deadline)?))?;
                if url.scheme() == "wss" {
                    let host = match host {
                        Host::Domain(name) => name.to_owned(),
                        Host::Ipv4(ip) => ip.to_string(),
                        Host::Ipv6(ip) => ip.to_string(),
                    };
                    let roots = native_roots()?;
                    RexStream::Tls(Box::new(connect_tls(tcp, host, roots, deadline)?))
                } else {
                    RexStream::Tcp(tcp)
                }
            }
        };
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        Ok(stream)
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "Rex connection timed out");
    Ok(remaining)
}

struct Lookup {
    host: String,
    port: u16,
    reply: mpsc::SyncSender<io::Result<Vec<SocketAddr>>>,
}

fn resolve(host: &str, port: u16, deadline: Instant) -> Result<Vec<SocketAddr>> {
    // System DNS can outlive its caller. One process-wide worker and one queued
    // request bound that cost even if reconnect repeatedly meets a stuck resolver.
    static RESOLVER: OnceLock<Result<mpsc::SyncSender<Lookup>, String>> = OnceLock::new();
    let resolver = RESOLVER
        .get_or_init(|| {
            let (send, receive) = mpsc::sync_channel::<Lookup>(1);
            thread::Builder::new()
                .name("rex-dns".into())
                .spawn(move || {
                    while let Ok(lookup) = receive.recv() {
                        let addresses = (lookup.host.as_str(), lookup.port)
                            .to_socket_addrs()
                            .map(|addresses| addresses.take(32).collect());
                        let _ = lookup.reply.try_send(addresses);
                    }
                })
                .map(|_| send)
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("start Rex DNS worker: {error}"))?;
    let (send, receive) = mpsc::sync_channel(1);
    resolver
        .try_send(Lookup {
            host: host.to_owned(),
            port,
            reply: send,
        })
        .context("Rex DNS resolver is busy or unavailable")?;
    receive
        .recv_timeout(remaining(deadline)?)
        .context("Rex DNS lookup timed out")?
        .context("resolve Rex host")
}

fn connect_tcp(addresses: Vec<SocketAddr>, deadline: Instant) -> Result<TcpStream> {
    let mut failure = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, remaining(deadline)?) {
            Ok(stream) => return Ok(stream),
            Err(error) => failure = Some(error),
        }
    }
    match failure {
        Some(error) => Err(error).context("connect to Rex TCP endpoint"),
        None => bail!("Rex host resolved to no network addresses"),
    }
}

fn native_roots() -> Result<RootCertStore> {
    let native = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let (accepted, _) = roots.add_parsable_certificates(native.certs);
    ensure!(accepted > 0, "no usable system CA certificates for Rex TLS");
    Ok(roots)
}

fn connect_tls(
    tcp: TcpStream,
    host: String,
    roots: RootCertStore,
    deadline: Instant,
) -> Result<StreamOwned<ClientConnection, TcpStream>> {
    let _deadline = HandshakeGuard::start(SocketInterrupt::Tcp(tcp.try_clone()?), deadline)?;
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
    let name = ServerName::try_from(host).context("invalid Rex TLS server name")?;
    let mut stream = StreamOwned::new(ClientConnection::new(Arc::new(config), name)?, tcp);
    while stream.conn.is_handshaking() {
        stream.sock.set_read_timeout(Some(remaining(deadline)?))?;
        stream.sock.set_write_timeout(Some(remaining(deadline)?))?;
        stream
            .conn
            .complete_io(&mut stream.sock)
            .context("verify Rex TLS certificate and handshake")?;
    }
    Ok(stream)
}

/// Transport ownership remains on the connection's IO thread, including TLS state.
pub enum RexStream {
    Unix(UnixStream),
    Tcp(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

/// A duplicated OS socket used only to interrupt an owned stream, never to send data.
pub enum SocketInterrupt {
    Unix(UnixStream),
    Tcp(TcpStream),
}

/// Enforces an absolute handshake budget even if a peer trickles bytes. The
/// guard owns only a duplicated descriptor; dropping it cancels the timer. Its
/// worker never joins a caller and lives no longer than the handshake budget.
pub(crate) struct HandshakeGuard(mpsc::SyncSender<()>);

impl HandshakeGuard {
    fn start(socket: SocketInterrupt, deadline: Instant) -> Result<Self> {
        let (send, receive) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("rex-handshake-deadline".into())
            .spawn(move || {
                if matches!(
                    receive.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    let _ = socket.shutdown(Shutdown::Both);
                }
            })
            .context("start Rex handshake deadline")?;
        Ok(Self(send))
    }
}

impl Drop for HandshakeGuard {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

impl SocketInterrupt {
    /// # Errors
    /// Reports operating-system shutdown errors.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.shutdown(how),
            Self::Tcp(stream) => stream.shutdown(how),
        }
    }
}

impl RexStream {
    pub(crate) fn handshake_guard(&self, timeout: Duration) -> Result<HandshakeGuard> {
        HandshakeGuard::start(self.interrupt_handle()?, Instant::now() + timeout)
    }

    /// Shut down the underlying socket without replaying buffered traffic.
    ///
    /// # Errors
    /// Reports operating-system shutdown errors.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.shutdown(how),
            Self::Tcp(stream) => stream.shutdown(how),
            Self::Tls(stream) => stream.sock.shutdown(how),
        }
    }

    /// # Errors
    /// Reports operating-system descriptor duplication errors.
    pub fn interrupt_handle(&self) -> io::Result<SocketInterrupt> {
        match self {
            Self::Unix(stream) => stream.try_clone().map(SocketInterrupt::Unix),
            Self::Tcp(stream) => stream.try_clone().map(SocketInterrupt::Tcp),
            Self::Tls(stream) => stream.sock.try_clone().map(SocketInterrupt::Tcp),
        }
    }

    /// # Errors
    /// Reports operating-system socket-option errors.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_read_timeout(timeout),
            Self::Tcp(stream) => stream.set_read_timeout(timeout),
            Self::Tls(stream) => stream.sock.set_read_timeout(timeout),
        }
    }

    /// # Errors
    /// Reports operating-system socket-option errors.
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_write_timeout(timeout),
            Self::Tcp(stream) => stream.set_write_timeout(timeout),
            Self::Tls(stream) => stream.sock.set_write_timeout(timeout),
        }
    }

    /// # Errors
    /// Reports operating-system socket-option errors.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_nonblocking(nonblocking),
            Self::Tcp(stream) => stream.set_nonblocking(nonblocking),
            Self::Tls(stream) => stream.sock.set_nonblocking(nonblocking),
        }
    }
}

impl Read for RexStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buffer),
            Self::Tcp(stream) => stream.read(buffer),
            Self::Tls(stream) => stream.read(buffer),
        }
    }
}

impl Write for RexStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(buffer),
            Self::Tcp(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{
        ServerConfig, ServerConnection,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    };
    use std::net::TcpListener;

    #[test]
    fn origins_preserve_host_and_select_fixed_control_and_data_paths() -> Result<()> {
        for (input, expected) in [
            ("http://localhost:1234", "ws://localhost:1234/api/control"),
            ("https://rex.example/", "wss://rex.example/api/control"),
            ("ws://127.0.0.1:42", "ws://127.0.0.1:42/api/control"),
            ("wss://[::1]:456/", "wss://[::1]:456/api/control"),
        ] {
            let endpoint = Endpoint::parse(input)?;
            assert_eq!(endpoint.websocket_url("/api/control")?, expected);
            assert!(endpoint.websocket_url("/api/data")?.ends_with("/api/data"));
            assert!(endpoint.websocket_url("/other").is_err());
        }
        assert_eq!(
            Endpoint::parse("unix:///tmp/rex.sock")?.websocket_url("/api/data")?,
            "ws://rex/api/data"
        );
        Ok(())
    }

    #[test]
    fn origins_reject_credentials_suffixes_and_unsupported_transports() {
        for input in [
            "unix://relative",
            "ftp://example.com",
            "https://user:secret@example.com",
            "wss://example.com?token=secret",
            "https://example.com/#secret",
            "http://example.com/api/control",
            "http://example.com/prefix/",
            "https://",
            "https://example.com\n",
            "tcp://127.0.0.1:1234",
        ] {
            assert!(Endpoint::parse(input).is_err(), "accepted invalid endpoint");
        }
    }

    #[test]
    fn tcp_stream_preserves_nonblocking_reads_and_interrupts() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let endpoint = Endpoint::parse(&format!("http://{}", listener.local_addr()?))?;
        let mut stream = endpoint.connect(Duration::from_secs(2))?;
        let (mut peer, _) = listener.accept()?;
        stream.set_nonblocking(true)?;
        assert_eq!(
            stream
                .read(&mut [0; 1])
                .err()
                .context("read unexpectedly completed")?
                .kind(),
            io::ErrorKind::WouldBlock
        );
        stream.set_nonblocking(false)?;
        stream.write_all(b"x")?;
        let mut byte = [0];
        peer.read_exact(&mut byte)?;
        assert_eq!(&byte, b"x");
        stream.interrupt_handle()?.shutdown(Shutdown::Both)?;
        assert_eq!(peer.read(&mut byte)?, 0);
        Ok(())
    }

    fn bytes(hex: &str) -> Result<Vec<u8>> {
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| Ok(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?))
            .collect()
    }

    fn tls_peer() -> Result<(SocketAddr, thread::JoinHandle<Result<()>>)> {
        let certificate = CertificateDer::from(bytes(TEST_CERTIFICATE)?);
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(bytes(TEST_PRIVATE_KEY)?));
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_single_cert(vec![certificate], key)?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let peer = thread::spawn(move || -> Result<()> {
            let (tcp, _) = listener.accept()?;
            tcp.set_read_timeout(Some(Duration::from_secs(2)))?;
            tcp.set_write_timeout(Some(Duration::from_secs(2)))?;
            let mut stream = StreamOwned::new(ServerConnection::new(Arc::new(config))?, tcp);
            let mut message = [0; 4];
            stream.read_exact(&mut message)?;
            ensure!(&message == b"ping", "unexpected TLS test request");
            stream.write_all(b"pong")?;
            stream.flush()?;
            Ok(())
        });
        Ok((address, peer))
    }

    fn test_roots() -> Result<RootCertStore> {
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(bytes(TEST_CERTIFICATE)?))?;
        Ok(roots)
    }

    #[test]
    fn tls_verifies_trusted_identity_and_preserves_stream_io() -> Result<()> {
        let (address, peer) = tls_peer()?;
        let tcp = TcpStream::connect(address)?;
        let tls = connect_tls(
            tcp,
            "localhost".into(),
            test_roots()?,
            Instant::now() + Duration::from_secs(2),
        )?;
        let mut stream = RexStream::Tls(Box::new(tls));
        stream.set_nonblocking(true)?;
        assert_eq!(
            stream
                .read(&mut [0; 1])
                .err()
                .context("read unexpectedly completed")?
                .kind(),
            io::ErrorKind::WouldBlock
        );
        stream.set_nonblocking(false)?;
        stream.write_all(b"ping")?;
        stream.flush()?;
        let mut response = [0; 4];
        stream.read_exact(&mut response)?;
        assert_eq!(&response, b"pong");
        peer.join()
            .map_err(|_| anyhow::anyhow!("TLS fixture panicked"))??;
        Ok(())
    }

    #[test]
    fn tls_rejects_wrong_hostname_and_untrusted_certificate() -> Result<()> {
        for roots in [Some(test_roots()?), None] {
            let (address, peer) = tls_peer()?;
            let result = if let Some(roots) = roots {
                connect_tls(
                    TcpStream::connect(address)?,
                    "wrong.example".into(),
                    roots,
                    Instant::now() + Duration::from_secs(2),
                )
                .map(|_| ())
            } else {
                Endpoint::parse(&format!("https://localhost:{}", address.port()))?
                    .connect(Duration::from_secs(2))
                    .map(|_| ())
            };
            assert!(result.is_err(), "TLS accepted an unverified identity");
            assert!(
                peer.join()
                    .map_err(|_| anyhow::anyhow!("TLS fixture panicked"))?
                    .is_err()
            );
        }
        Ok(())
    }
    #[test]
    fn handshake_deadline_interrupts_trickled_io_and_cancels_on_success() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let mut client = RexStream::Tcp(TcpStream::connect(listener.local_addr()?)?);
        let (mut peer, _) = listener.accept()?;
        let guard = client.handshake_guard(Duration::from_millis(30))?;
        let producer = thread::spawn(move || {
            for _ in 0..200 {
                if peer.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        let mut byte = [0];
        while client.read(&mut byte).is_ok_and(|count| count > 0) {}
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(guard);
        producer
            .join()
            .map_err(|_| anyhow::anyhow!("trickle peer panicked"))?;

        let mut client = RexStream::Tcp(TcpStream::connect(listener.local_addr()?)?);
        let (mut peer, _) = listener.accept()?;
        let guard = client.handshake_guard(Duration::from_millis(10))?;
        drop(guard);
        thread::sleep(Duration::from_millis(30));
        client.set_read_timeout(Some(Duration::from_secs(1)))?;
        peer.write_all(b"z")?;
        client.read_exact(&mut byte)?;
        assert_eq!(&byte, b"z");
        Ok(())
    }

    // Public, self-signed localhost test fixture, valid 2026–2036. This key is
    // deliberately embedded for loopback tests and is never used in production.
    const TEST_CERTIFICATE: &str = "308201a43082014ba0030201020214305c0a652a0f5121f6680088285faf3815865000300a06082a8648ce3d04030230143112301006035504030c096c6f63616c686f7374301e170d3236313032393034313831305a170d3336313032363034313831305a30143112301006035504030c096c6f63616c686f73743059301306072a8648ce3d020106082a8648ce3d03010703420004b9bbcd3943853d5621c2ba569b172b87b480e46ef22023b33e6c82ba35309761bf23f8b346131388043ddbf37fb18adb95e7bb8db739454006ef428ef114dadaa37b3079301d0603551d0e04160414a0ff3f3f9e4c57c597f6b402e65c601a26a45348301f0603551d23041830168014a0ff3f3f9e4c57c597f6b402e65c601a26a4534830140603551d11040d300b82096c6f63616c686f7374300c0603551d130101ff0402300030130603551d25040c300a06082b06010505070301300a06082a8648ce3d0403020347003044022074e8dd61238e83edf121b388d6fb1ee1c0ac9f1ca0e4f10cd0ea2d13619b5fd602203eeb62bb90c7e609b9fac1ef64e6c02ba27ec5df7596a8993e77bbcebac77af5";
    const TEST_PRIVATE_KEY: &str = "308187020100301306072a8648ce3d020106082a8648ce3d030107046d306b02010104205bdd9fd8455d5e56cc227e4a67ee9bbd0587a7bc0ab1dbb115c81b7f3fb54f4aa14403420004b9bbcd3943853d5621c2ba569b172b87b480e46ef22023b33e6c82ba35309761bf23f8b346131388043ddbf37fb18adb95e7bb8db739454006ef428ef114dada";
}
