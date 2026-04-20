use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{lookup_host, TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use aivpn_common::error::{Error, Result};
use crate::netns::NetworkNamespace;

const SOCKS5_VERSION: u8 = 0x05;
const SOCKS5_METHOD_NO_AUTH: u8 = 0x00;
const SOCKS5_METHOD_NO_ACCEPTABLE: u8 = 0xff;
const SOCKS5_CMD_CONNECT: u8 = 0x01;
const SOCKS5_CMD_UDP_ASSOCIATE: u8 = 0x03;
const SOCKS5_ATYP_IPV4: u8 = 0x01;
const SOCKS5_ATYP_DOMAIN: u8 = 0x03;
const SOCKS5_ATYP_IPV6: u8 = 0x04;
const SOCKS5_REPLY_SUCCEEDED: u8 = 0x00;
const SOCKS5_REPLY_GENERAL_FAILURE: u8 = 0x01;
const SOCKS5_REPLY_CONNECTION_NOT_ALLOWED: u8 = 0x02;
const SOCKS5_REPLY_NETWORK_UNREACHABLE: u8 = 0x03;
const SOCKS5_REPLY_HOST_UNREACHABLE: u8 = 0x04;
const SOCKS5_REPLY_CONNECTION_REFUSED: u8 = 0x05;
const SOCKS5_REPLY_COMMAND_NOT_SUPPORTED: u8 = 0x07;
const SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;
const DEFAULT_LOCAL_SOCKS5_MAX_CLIENTS: usize = 256;
const DEFAULT_LOCAL_SOCKS5_MAX_CONCURRENT_DIALS: usize = 128;
const LOCAL_SOCKS5_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
const LOCAL_SOCKS5_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const LOCAL_SOCKS5_QUEUE_LOG_THRESHOLD: Duration = Duration::from_millis(250);
const LOCAL_SOCKS5_SLOW_CONNECT_LOG_THRESHOLD: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocalSocks5Config {
    #[serde(default = "default_local_socks5_host")]
    pub host: String,
    #[serde(default = "default_local_socks5_port")]
    pub port: u16,
    #[serde(default = "default_local_socks5_max_clients")]
    pub max_clients: usize,
    #[serde(default = "default_local_socks5_max_concurrent_dials")]
    pub max_concurrent_dials: usize,
}

fn default_local_socks5_host() -> String {
    "127.0.0.1".to_string()
}

fn default_local_socks5_port() -> u16 {
    1080
}

fn default_local_socks5_max_clients() -> usize {
    DEFAULT_LOCAL_SOCKS5_MAX_CLIENTS
}

fn default_local_socks5_max_concurrent_dials() -> usize {
    DEFAULT_LOCAL_SOCKS5_MAX_CONCURRENT_DIALS
}

impl Default for LocalSocks5Config {
    fn default() -> Self {
        Self {
            host: default_local_socks5_host(),
            port: default_local_socks5_port(),
            max_clients: default_local_socks5_max_clients(),
            max_concurrent_dials: default_local_socks5_max_concurrent_dials(),
        }
    }
}

impl LocalSocks5Config {
    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            return Err(Error::Session("Local SOCKS5 host cannot be empty".into()));
        }
        if self.max_clients == 0 {
            return Err(Error::Session(
                "Local SOCKS5 max_clients must be greater than zero".into(),
            ));
        }
        if self.max_concurrent_dials == 0 {
            return Err(Error::Session(
                "Local SOCKS5 max_concurrent_dials must be greater than zero".into(),
            ));
        }

        #[cfg(not(target_os = "linux"))]
        {
            return Err(Error::Session(
                "Local SOCKS5 mode is currently supported on Linux only".into(),
            ));
        }

        #[cfg(target_os = "linux")]
        {
            Ok(())
        }
    }

    pub fn display_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[derive(Debug)]
pub struct LocalSocks5Runtime {
    ready: AtomicBool,
    namespace: RwLock<Option<Arc<NetworkNamespace>>>,
    dial_slots: Arc<Semaphore>,
    max_concurrent_dials: usize,
    next_session_id: portable_atomic::AtomicU64,
    generation: portable_atomic::AtomicU64,
    reset_notify: Notify,
}

#[derive(Debug)]
struct ConnectTargetSuccess {
    stream: TcpStream,
    setup_elapsed: Duration,
    connect_wait_elapsed: Duration,
}

#[derive(Debug)]
struct ConnectTargetFailure {
    error: Error,
    setup_elapsed: Duration,
    connect_wait_elapsed: Duration,
}

impl LocalSocks5Runtime {
    pub fn new(max_concurrent_dials: usize) -> Self {
        Self {
            ready: AtomicBool::new(false),
            namespace: RwLock::new(None),
            dial_slots: Arc::new(Semaphore::new(max_concurrent_dials)),
            max_concurrent_dials,
            next_session_id: portable_atomic::AtomicU64::new(1),
            generation: portable_atomic::AtomicU64::new(1),
            reset_notify: Notify::new(),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    pub fn set_namespace(&self, namespace: Option<Arc<NetworkNamespace>>) {
        if let Ok(mut guard) = self.namespace.write() {
            *guard = namespace;
        }
    }

    pub fn namespace(&self) -> Result<Arc<NetworkNamespace>> {
        self.namespace
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
            .ok_or_else(|| Error::Session("Local SOCKS5 namespace is not initialized".into()))
    }

    pub fn next_session_id(&self) -> u64 {
        self.next_session_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn available_dial_slots(&self) -> usize {
        self.dial_slots.available_permits()
    }

    pub fn max_concurrent_dials(&self) -> usize {
        self.max_concurrent_dials
    }

    pub fn reset_active_sessions(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.reset_notify.notify_waiters();
    }

    pub async fn wait_for_generation_change(&self, generation: u64) {
        loop {
            let notified = self.reset_notify.notified();
            if self.current_generation() != generation {
                return;
            }
            notified.await;
        }
    }

    pub async fn acquire_dial_slot(&self, generation: u64) -> Result<OwnedSemaphorePermit> {
        if self.current_generation() != generation {
            return Err(Error::Session(
                "Local SOCKS5 dial cancelled by tunnel reset".into(),
            ));
        }

        tokio::select! {
            permit = self.dial_slots.clone().acquire_owned() => permit
                .map_err(|_| Error::Session("Local SOCKS5 dial queue is shutting down".into())),
            _ = self.wait_for_generation_change(generation) => Err(Error::Session(
                "Local SOCKS5 dial cancelled by tunnel reset".into(),
            )),
        }
    }
}

#[derive(Debug, Clone)]
enum TargetAddr {
    Socket(SocketAddr),
    Domain(String, u16),
}

impl TargetAddr {
    fn display(&self) -> String {
        match self {
            Self::Socket(addr) => addr.to_string(),
            Self::Domain(host, port) => format!("{host}:{port}"),
        }
    }
}

pub async fn spawn_local_socks5_server(
    config: LocalSocks5Config,
    runtime: Arc<LocalSocks5Runtime>,
) -> Result<JoinHandle<Result<()>>> {
    config.validate()?;
    let bind_addr = resolve_bind_addr(&config).await?;
    let listener = TcpListener::bind(bind_addr).await.map_err(Error::Io)?;
    let actual_addr = listener.local_addr().map_err(Error::Io)?;
    let max_clients = config.max_clients;
    let client_slots = Arc::new(Semaphore::new(max_clients));
    info!(
        "Local SOCKS5 server listening on {} (max_clients {}, max_concurrent_dials {})",
        actual_addr,
        max_clients,
        runtime.max_concurrent_dials()
    );

    Ok(tokio::spawn(async move {
        loop {
            let (stream, peer_addr) = match listener.accept().await {
                Ok(conn) => conn,
                Err(err) => {
                    warn!(
                        "Local SOCKS5 accept failed on {}: {}. Listener will stay up and retry",
                        actual_addr, err
                    );
                    sleep(Duration::from_millis(250)).await;
                    continue;
                }
            };

            let Ok(permit) = client_slots.clone().try_acquire_owned() else {
                warn!(
                    "Local SOCKS5 is at capacity on {} ({} active clients); dropping {}",
                    actual_addr,
                    max_clients,
                    peer_addr
                );
                drop(stream);
                continue;
            };

            let runtime = runtime.clone();
            let session_id = runtime.next_session_id();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(err) = handle_client(stream, peer_addr, runtime, session_id).await {
                    if is_benign_client_disconnect(&err) {
                        debug!(
                            "Local SOCKS5 session #{} client {} closed: {}",
                            session_id, peer_addr, err
                        );
                    } else {
                        debug!(
                            "Local SOCKS5 session #{} client {} ended with error: {}",
                            session_id, peer_addr, err
                        );
                    }
                }
            });
        }
    }))
}

fn is_benign_client_disconnect(err: &Error) -> bool {
    match err {
        Error::Io(io_err) => matches!(
            io_err.kind(),
            io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::UnexpectedEof
        ),
        _ => false,
    }
}

async fn resolve_bind_addr(config: &LocalSocks5Config) -> Result<SocketAddr> {
    lookup_host((config.host.as_str(), config.port))
        .await
        .map_err(Error::Io)?
        .next()
        .ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                format!("Could not resolve local SOCKS5 bind address {}", config.display_addr()),
            ))
        })
}

async fn handle_client(
    mut client: TcpStream,
    peer_addr: SocketAddr,
    runtime: Arc<LocalSocks5Runtime>,
    session_id: u64,
) -> Result<()> {
    let (command, target) = match timeout(
        LOCAL_SOCKS5_HANDSHAKE_TIMEOUT,
        read_request(&mut client),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(Error::Session(format!(
                "Local SOCKS5 handshake timed out after {}s",
                LOCAL_SOCKS5_HANDSHAKE_TIMEOUT.as_secs()
            )));
        }
    };
    let session_generation = runtime.current_generation();

    match command {
        SOCKS5_CMD_CONNECT => {
            handle_connect(
                &mut client,
                target,
                peer_addr,
                runtime,
                session_id,
                session_generation,
            )
            .await
        }
        SOCKS5_CMD_UDP_ASSOCIATE => {
            handle_udp_associate(
                &mut client,
                target,
                peer_addr,
                runtime,
                session_id,
                session_generation,
            )
            .await
        }
        _ => {
            let reply_addr = unspecified_addr_for_peer(peer_addr);
            send_reply(&mut client, SOCKS5_REPLY_COMMAND_NOT_SUPPORTED, reply_addr).await?;
            Err(Error::Session(format!(
                "Unsupported local SOCKS5 command 0x{command:02x}"
            )))
        }
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<(u8, TargetAddr)> {
    perform_method_negotiation(stream).await?;

    let mut request_header = [0u8; 4];
    stream
        .read_exact(&mut request_header)
        .await
        .map_err(Error::Io)?;

    if request_header[0] != SOCKS5_VERSION {
        return Err(Error::Session(format!(
            "Invalid SOCKS5 request version 0x{:02x}",
            request_header[0]
        )));
    }

    let command = request_header[1];
    let target = read_target_addr(stream, request_header[3]).await?;
    Ok((command, target))
}

async fn perform_method_negotiation(stream: &mut TcpStream) -> Result<()> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await.map_err(Error::Io)?;

    if header[0] != SOCKS5_VERSION {
        return Err(Error::Session(format!(
            "Invalid SOCKS5 handshake version 0x{:02x}",
            header[0]
        )));
    }

    let method_count = header[1] as usize;
    let mut methods = vec![0u8; method_count];
    stream.read_exact(&mut methods).await.map_err(Error::Io)?;

    let selected = if methods.contains(&SOCKS5_METHOD_NO_AUTH) {
        SOCKS5_METHOD_NO_AUTH
    } else {
        SOCKS5_METHOD_NO_ACCEPTABLE
    };

    stream
        .write_all(&[SOCKS5_VERSION, selected])
        .await
        .map_err(Error::Io)?;

    if selected == SOCKS5_METHOD_NO_ACCEPTABLE {
        return Err(Error::Session(
            "Local SOCKS5 client did not offer no-auth method".into(),
        ));
    }

    Ok(())
}

async fn handle_connect(
    client: &mut TcpStream,
    target: TargetAddr,
    peer_addr: SocketAddr,
    runtime: Arc<LocalSocks5Runtime>,
    session_id: u64,
    session_generation: u64,
) -> Result<()> {
    let target_display = target.display();
    debug!(
        "Local SOCKS5 session #{} CONNECT {} -> {}",
        session_id, peer_addr, target_display
    );

    if !runtime.is_ready() {
        let reply_addr = unspecified_addr_for_peer(peer_addr);
        send_reply(client, SOCKS5_REPLY_NETWORK_UNREACHABLE, reply_addr).await?;
        return Err(Error::Session(
            "AIVPN tunnel unavailable for local SOCKS5 CONNECT".into(),
        ));
    }

    let queue_started = Instant::now();
    let available_before_queue = runtime.available_dial_slots();
    let dial_permit = runtime.acquire_dial_slot(session_generation).await?;
    let queue_wait = queue_started.elapsed();
    if queue_wait >= LOCAL_SOCKS5_QUEUE_LOG_THRESHOLD || available_before_queue == 0 {
        info!(
            "Local SOCKS5 session #{} waited {:?} in dial queue for {} (peer {}, {} dial slots available before wait)",
            session_id,
            queue_wait,
            target_display,
            peer_addr,
            available_before_queue
        );
    }

    let dial_started = Instant::now();
    match connect_target(target.clone(), runtime.clone(), session_generation).await {
        Ok(connect_res) => {
            let mut upstream = connect_res.stream;
            let dial_elapsed = dial_started.elapsed();
            let bind_addr = upstream.local_addr().map_err(Error::Io)?;
            if dial_elapsed >= LOCAL_SOCKS5_SLOW_CONNECT_LOG_THRESHOLD
                || queue_wait >= LOCAL_SOCKS5_QUEUE_LOG_THRESHOLD
            {
                info!(
                    "Local SOCKS5 session #{} connected {} in {:?} after queue wait {:?} (setup {:?}, connect wait {:?}, peer {}, bind {})",
                    session_id,
                    target_display,
                    dial_elapsed,
                    queue_wait,
                    connect_res.setup_elapsed,
                    connect_res.connect_wait_elapsed,
                    peer_addr,
                    bind_addr
                );
            }
            // `max_concurrent_dials` is intended to cap simultaneous upstream
            // connect attempts, not the lifetime of established proxied
            // connections. Active client sessions are already bounded by
            // `max_clients`, so release the dial slot before starting relay I/O.
            drop(dial_permit);
            send_reply(client, SOCKS5_REPLY_SUCCEEDED, bind_addr).await?;
            tokio::select! {
                relay_res = copy_bidirectional(client, &mut upstream) => {
                    let _ = relay_res.map_err(Error::Io)?;
                    Ok(())
                }
                _ = runtime.wait_for_generation_change(session_generation) => {
                    let _ = upstream.shutdown().await;
                    let _ = client.shutdown().await;
                    Err(Error::Session(
                        "AIVPN tunnel reset during local SOCKS5 relay".into(),
                    ))
                }
            }
        }
        Err(connect_err) => {
            let dial_elapsed = dial_started.elapsed();
            let reply = map_error_to_reply(&connect_err.error);
            let reply_addr = unspecified_addr_for_peer(peer_addr);
            let _ = send_reply(client, reply, reply_addr).await;
            warn!(
                "Local SOCKS5 session #{} failed dialing {} after queue {:?} and dial {:?} (setup {:?}, connect wait {:?}): {}",
                session_id,
                target_display,
                queue_wait,
                dial_elapsed,
                connect_err.setup_elapsed,
                connect_err.connect_wait_elapsed,
                connect_err.error
            );
            drop(dial_permit);
            Err(connect_err.error)
        }
    }
}

async fn handle_udp_associate(
    client: &mut TcpStream,
    target: TargetAddr,
    peer_addr: SocketAddr,
    runtime: Arc<LocalSocks5Runtime>,
    session_id: u64,
    session_generation: u64,
) -> Result<()> {
    debug!(
        "Local SOCKS5 session #{} UDP ASSOCIATE {} -> {}",
        session_id,
        peer_addr,
        target.display()
    );

    if !runtime.is_ready() {
        let reply_addr = unspecified_addr_for_peer(peer_addr);
        send_reply(client, SOCKS5_REPLY_NETWORK_UNREACHABLE, reply_addr).await?;
        return Err(Error::Session(
            "AIVPN tunnel unavailable for local SOCKS5 UDP ASSOCIATE".into(),
        ));
    }

    let client_bind_addr = client.local_addr().map_err(Error::Io)?;
    let relay_client = UdpSocket::bind(SocketAddr::new(client_bind_addr.ip(), 0))
        .await
        .map_err(Error::Io)?;
    let relay_reply_addr = relay_client.local_addr().map_err(Error::Io)?;

    let relay_upstream = create_namespace_udp_socket(client_bind_addr.is_ipv4(), runtime.clone()).await?;
    let relay_upstream_addr = relay_upstream.local_addr().map_err(Error::Io)?;
    debug!(
        "Local SOCKS5 UDP relay client={} upstream={}",
        relay_reply_addr, relay_upstream_addr
    );

    send_reply(client, SOCKS5_REPLY_SUCCEEDED, relay_reply_addr).await?;

    let mut client_udp_addr = match target {
        TargetAddr::Socket(addr) if addr.port() != 0 => Some(addr),
        _ => None,
    };
    let mut control_buf = [0u8; 1];
    let mut client_buf = vec![0u8; 65_535];
    let mut upstream_buf = vec![0u8; 65_535];

    loop {
        tokio::select! {
            _ = runtime.wait_for_generation_change(session_generation) => {
                return Err(Error::Session(
                    "AIVPN tunnel reset during local SOCKS5 UDP relay".into(),
                ));
            }
            control_res = client.read(&mut control_buf) => {
                match control_res {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(err) => return Err(Error::Io(err)),
                }
            }
            client_res = relay_client.recv_from(&mut client_buf) => {
                let (len, source_addr) = client_res.map_err(Error::Io)?;
                let is_client_packet = source_addr.ip() == peer_addr.ip()
                    && match client_udp_addr {
                        Some(client_addr) => client_addr == source_addr,
                        None => true,
                    };

                if !is_client_packet {
                    continue;
                }

                if !runtime.is_ready() {
                    return Err(Error::Session(
                        "AIVPN tunnel unavailable for local SOCKS5 UDP relay".into(),
                    ));
                }

                client_udp_addr = Some(source_addr);
                let (target, payload) = parse_udp_packet(&client_buf[..len])?;
                let upstream_addr = resolve_target_addr(&target).await?;
                relay_upstream
                    .send_to(payload, upstream_addr)
                    .await
                    .map_err(Error::Io)?;
            }
            upstream_res = relay_upstream.recv_from(&mut upstream_buf) => {
                let (len, source_addr) = upstream_res.map_err(Error::Io)?;
                if let Some(client_addr) = client_udp_addr {
                    let packet = build_udp_packet(source_addr, &upstream_buf[..len]);
                    relay_client
                        .send_to(&packet, client_addr)
                        .await
                        .map_err(Error::Io)?;
                }
            }
        }
    }

    Ok(())
}

fn is_nonblocking_connect_in_progress(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
        || matches!(
            err.raw_os_error(),
            Some(code) if code == libc::EINPROGRESS || code == libc::EALREADY
        )
}

async fn connect_target(
    target: TargetAddr,
    runtime: Arc<LocalSocks5Runtime>,
    generation: u64,
) -> std::result::Result<ConnectTargetSuccess, ConnectTargetFailure> {
    let target_addr = match &target {
        TargetAddr::Socket(addr) => *addr,
        TargetAddr::Domain(host, port) => {
            resolve_target_addr(&TargetAddr::Domain(host.clone(), *port))
                .await
                .map_err(|error| ConnectTargetFailure {
                    error,
                    setup_elapsed: Duration::ZERO,
                    connect_wait_elapsed: Duration::ZERO,
                })?
        }
    };

    let namespace = runtime.namespace().map_err(|error| ConnectTargetFailure {
        error,
        setup_elapsed: Duration::ZERO,
        connect_wait_elapsed: Duration::ZERO,
    })?;
    let target_display = target.display();
    let (std_stream, setup_elapsed) = tokio::task::spawn_blocking(move || {
        let setup_started = Instant::now();
        namespace.run(|| {
            let domain = if target_addr.is_ipv4() {
                socket2::Domain::IPV4
            } else {
                socket2::Domain::IPV6
            };
            let socket = socket2::Socket::new(
                domain,
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )
            .map_err(Error::Io)?;
            socket.set_nonblocking(true).map_err(Error::Io)?;
            match socket.connect(&target_addr.into()) {
                Ok(()) => {}
                Err(err) if is_nonblocking_connect_in_progress(&err) => {}
                Err(err) => return Err(Error::Io(err)),
            }
            let stream: std::net::TcpStream = socket.into();
            stream.set_nonblocking(true).map_err(Error::Io)?;
            Ok((stream, setup_started.elapsed()))
        })
    })
    .await
    .map_err(|err| ConnectTargetFailure {
        error: Error::Session(format!("Local SOCKS5 connect task failed: {err}")),
        setup_elapsed: Duration::ZERO,
        connect_wait_elapsed: Duration::ZERO,
    })?
    .map_err(|error| ConnectTargetFailure {
        error,
        setup_elapsed: Duration::ZERO,
        connect_wait_elapsed: Duration::ZERO,
    })?;

    let stream = TcpStream::from_std(std_stream).map_err(|err| ConnectTargetFailure {
        error: Error::Io(err),
        setup_elapsed,
        connect_wait_elapsed: Duration::ZERO,
    })?;

    let connect_wait_started = Instant::now();
    let connect_wait_result: Result<()> = timeout(LOCAL_SOCKS5_CONNECT_TIMEOUT, async {
        tokio::select! {
            writable_res = stream.writable() => writable_res.map_err(Error::Io),
            _ = runtime.wait_for_generation_change(generation) => Err(Error::Session(
                "Local SOCKS5 dial cancelled by tunnel reset".into(),
            )),
        }
    })
        .await
        .map_err(|_| ConnectTargetFailure {
            error: Error::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Timed out connecting to SOCKS5 target {} after {}s",
                    target_display,
                    LOCAL_SOCKS5_CONNECT_TIMEOUT.as_secs()
                ),
            )),
            setup_elapsed,
            connect_wait_elapsed: LOCAL_SOCKS5_CONNECT_TIMEOUT,
        })?;
    connect_wait_result.map_err(|error| ConnectTargetFailure {
            error,
            setup_elapsed,
            connect_wait_elapsed: connect_wait_started.elapsed(),
        })?;

    if let Some(err) = stream.take_error().map_err(|err| ConnectTargetFailure {
        error: Error::Io(err),
        setup_elapsed,
        connect_wait_elapsed: connect_wait_started.elapsed(),
    })? {
        return Err(ConnectTargetFailure {
            error: Error::Io(err),
            setup_elapsed,
            connect_wait_elapsed: connect_wait_started.elapsed(),
        });
    }

    Ok(ConnectTargetSuccess {
        stream,
        setup_elapsed,
        connect_wait_elapsed: connect_wait_started.elapsed(),
    })
}

async fn create_namespace_udp_socket(
    ipv4: bool,
    runtime: Arc<LocalSocks5Runtime>,
) -> Result<UdpSocket> {
    let namespace = runtime.namespace()?;
    let std_socket = tokio::task::spawn_blocking(move || {
        namespace.run(|| {
            let domain = if ipv4 {
                socket2::Domain::IPV4
            } else {
                socket2::Domain::IPV6
            };

            let socket = socket2::Socket::new(
                domain,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )
            .map_err(Error::Io)?;

            socket.set_nonblocking(true).map_err(Error::Io)?;

            let bind_addr: SocketAddr = if ipv4 {
                "0.0.0.0:0".parse().expect("valid IPv4 wildcard address")
            } else {
                "[::]:0".parse().expect("valid IPv6 wildcard address")
            };
            socket.bind(&bind_addr.into()).map_err(Error::Io)?;

            let std_socket: std::net::UdpSocket = socket.into();
            Ok(std_socket)
        })
    })
    .await
    .map_err(|err| Error::Session(format!("Local SOCKS5 UDP task failed: {err}")))??;

    UdpSocket::from_std(std_socket).map_err(Error::Io)
}

async fn resolve_target_addr(target: &TargetAddr) -> Result<SocketAddr> {
    match target {
        TargetAddr::Socket(addr) => Ok(*addr),
        TargetAddr::Domain(host, port) => lookup_host((host.as_str(), *port))
            .await
            .map_err(Error::Io)?
            .next()
            .ok_or_else(|| {
                Error::Io(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    format!("Could not resolve SOCKS5 target {host}:{port}"),
                ))
            }),
    }
}

async fn read_target_addr(stream: &mut TcpStream, atyp: u8) -> Result<TargetAddr> {
    match atyp {
        SOCKS5_ATYP_IPV4 => {
            let mut addr = [0u8; 4];
            let mut port = [0u8; 2];
            stream.read_exact(&mut addr).await.map_err(Error::Io)?;
            stream.read_exact(&mut port).await.map_err(Error::Io)?;
            Ok(TargetAddr::Socket(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(addr)),
                u16::from_be_bytes(port),
            )))
        }
        SOCKS5_ATYP_IPV6 => {
            let mut addr = [0u8; 16];
            let mut port = [0u8; 2];
            stream.read_exact(&mut addr).await.map_err(Error::Io)?;
            stream.read_exact(&mut port).await.map_err(Error::Io)?;
            Ok(TargetAddr::Socket(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(addr)),
                u16::from_be_bytes(port),
            )))
        }
        SOCKS5_ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(Error::Io)?;
            let mut host = vec![0u8; len[0] as usize];
            let mut port = [0u8; 2];
            stream.read_exact(&mut host).await.map_err(Error::Io)?;
            stream.read_exact(&mut port).await.map_err(Error::Io)?;
            let host = String::from_utf8(host).map_err(|_| {
                Error::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SOCKS5 target contains invalid UTF-8 domain bytes",
                ))
            })?;
            Ok(TargetAddr::Domain(host, u16::from_be_bytes(port)))
        }
        _ => Err(Error::Session(format!(
            "Unsupported SOCKS5 target address type 0x{atyp:02x}"
        ))),
    }
}

async fn send_reply(stream: &mut TcpStream, reply_code: u8, bind_addr: SocketAddr) -> Result<()> {
    let mut reply = Vec::with_capacity(22);
    reply.push(SOCKS5_VERSION);
    reply.push(reply_code);
    reply.push(0x00);
    reply.extend_from_slice(&encode_socket_addr(bind_addr));
    stream.write_all(&reply).await.map_err(Error::Io)
}

fn encode_socket_addr(addr: SocketAddr) -> Vec<u8> {
    match addr {
        SocketAddr::V4(addr) => {
            let mut buf = Vec::with_capacity(1 + 4 + 2);
            buf.push(SOCKS5_ATYP_IPV4);
            buf.extend_from_slice(&addr.ip().octets());
            buf.extend_from_slice(&addr.port().to_be_bytes());
            buf
        }
        SocketAddr::V6(addr) => {
            let mut buf = Vec::with_capacity(1 + 16 + 2);
            buf.push(SOCKS5_ATYP_IPV6);
            buf.extend_from_slice(&addr.ip().octets());
            buf.extend_from_slice(&addr.port().to_be_bytes());
            buf
        }
    }
}

fn parse_udp_packet(packet: &[u8]) -> Result<(TargetAddr, &[u8])> {
    if packet.len() < 4 {
        return Err(Error::InvalidPacket("SOCKS5 UDP packet too short"));
    }
    if packet[0] != 0x00 || packet[1] != 0x00 {
        return Err(Error::InvalidPacket("SOCKS5 UDP packet has invalid RSV"));
    }
    if packet[2] != 0x00 {
        return Err(Error::InvalidPacket("SOCKS5 UDP fragmentation is unsupported"));
    }

    let (target, header_len) = match packet[3] {
        SOCKS5_ATYP_IPV4 => {
            if packet.len() < 10 {
                return Err(Error::InvalidPacket("SOCKS5 UDP IPv4 packet too short"));
            }
            let addr = Ipv4Addr::new(packet[4], packet[5], packet[6], packet[7]);
            let port = u16::from_be_bytes([packet[8], packet[9]]);
            (
                TargetAddr::Socket(SocketAddr::new(IpAddr::V4(addr), port)),
                10,
            )
        }
        SOCKS5_ATYP_IPV6 => {
            if packet.len() < 22 {
                return Err(Error::InvalidPacket("SOCKS5 UDP IPv6 packet too short"));
            }
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&packet[4..20]);
            let port = u16::from_be_bytes([packet[20], packet[21]]);
            (
                TargetAddr::Socket(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(addr)), port)),
                22,
            )
        }
        SOCKS5_ATYP_DOMAIN => {
            let Some(host_len) = packet.get(4) else {
                return Err(Error::InvalidPacket("SOCKS5 UDP domain packet missing length"));
            };
            let host_end = 5 + *host_len as usize;
            if packet.len() < host_end + 2 {
                return Err(Error::InvalidPacket("SOCKS5 UDP domain packet too short"));
            }
            let host = String::from_utf8(packet[5..host_end].to_vec()).map_err(|_| {
                Error::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SOCKS5 UDP target contains invalid UTF-8 domain bytes",
                ))
            })?;
            let port = u16::from_be_bytes([packet[host_end], packet[host_end + 1]]);
            (TargetAddr::Domain(host, port), host_end + 2)
        }
        _ => {
            return Err(Error::InvalidPacket(
                "SOCKS5 UDP packet has invalid address type",
            ));
        }
    };

    Ok((target, &packet[header_len..]))
}

fn build_udp_packet(source_addr: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let addr = encode_socket_addr(source_addr);
    let mut packet = Vec::with_capacity(3 + addr.len() + payload.len());
    packet.extend_from_slice(&[0x00, 0x00, 0x00]);
    packet.extend_from_slice(&addr);
    packet.extend_from_slice(payload);
    packet
}

fn map_error_to_reply(err: &Error) -> u8 {
    match err {
        Error::Io(io_err) => match io_err.kind() {
            io::ErrorKind::ConnectionRefused => SOCKS5_REPLY_CONNECTION_REFUSED,
            io::ErrorKind::PermissionDenied => SOCKS5_REPLY_CONNECTION_NOT_ALLOWED,
            io::ErrorKind::HostUnreachable | io::ErrorKind::TimedOut => {
                SOCKS5_REPLY_HOST_UNREACHABLE
            }
            io::ErrorKind::NetworkUnreachable | io::ErrorKind::NetworkDown => {
                SOCKS5_REPLY_NETWORK_UNREACHABLE
            }
            io::ErrorKind::AddrNotAvailable | io::ErrorKind::InvalidInput => {
                SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED
            }
            _ => SOCKS5_REPLY_GENERAL_FAILURE,
        },
        Error::Session(message) if message.contains("address type") => {
            SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED
        }
        Error::Session(message)
            if message.contains("tunnel")
                || message.contains("namespace")
                || message.contains("reset") =>
        {
            SOCKS5_REPLY_NETWORK_UNREACHABLE
        }
        _ => SOCKS5_REPLY_GENERAL_FAILURE,
    }
}

fn unspecified_addr_for_peer(peer_addr: SocketAddr) -> SocketAddr {
    if peer_addr.is_ipv4() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
    } else {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_udp_packet_for_ipv4_payload() {
        let mut packet = vec![0x00, 0x00, 0x00, SOCKS5_ATYP_IPV4, 1, 2, 3, 4];
        packet.extend_from_slice(&443u16.to_be_bytes());
        packet.extend_from_slice(b"hello");

        let (target, payload) = parse_udp_packet(&packet).unwrap();
        assert_eq!(payload, b"hello");
        match target {
            TargetAddr::Socket(addr) => assert_eq!(addr, "1.2.3.4:443".parse().unwrap()),
            other => panic!("expected socket target, got {other:?}"),
        }
    }

    #[test]
    fn local_socks5_config_rejects_empty_host() {
        let config = LocalSocks5Config {
            host: " ".into(),
            port: 1080,
            max_clients: default_local_socks5_max_clients(),
            max_concurrent_dials: default_local_socks5_max_concurrent_dials(),
        };

        assert!(config.validate().is_err());
    }
}
