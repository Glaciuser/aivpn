//! AIVPN client binary.

use std::fs;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use aivpn_client::AivpnClient;
use aivpn_client::client::{ClientConfig, ClientMode};
use aivpn_client::local_socks::{
    LocalSocks5Config, LocalSocks5Runtime, spawn_local_socks5_server,
};
use aivpn_client::tunnel::TunnelConfig;
use aivpn_common::error::{Error, Result};
use aivpn_common::mask::preset_masks::webrtc_zoom_v3;
use aivpn_common::network_config::{
    ClientNetworkConfig, DEFAULT_VPN_MTU, LEGACY_SERVER_VPN_IP,
};
use base64::Engine;
use clap::{ArgAction, Parser, ValueEnum};
use rand::Rng;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

const INITIAL_RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RuntimeMode {
    Tun,
    Socks5,
}

impl RuntimeMode {
    fn as_client_mode(self) -> ClientMode {
        match self {
            Self::Tun => ClientMode::Tun,
            Self::Socks5 => ClientMode::Socks5,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Tun => "tun",
            Self::Socks5 => "socks5",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_filter(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct ClientArgs {
    /// Server address, for example 1.2.3.4:443
    #[arg(short, long)]
    server: Option<String>,

    /// Server public key, base64 encoded 32-byte X25519 key
    #[arg(long)]
    server_key: Option<String>,

    /// Connection key, for example aivpn://...
    #[arg(short = 'k', long)]
    connection_key: Option<String>,

    /// Mode: system TUN client or local SOCKS5 proxy
    #[arg(long, value_enum)]
    mode: Option<RuntimeMode>,

    /// Fixed TUN device name; generated once per process when omitted
    #[arg(long)]
    tun_name: Option<String>,

    /// Fallback TUN client IPv4 address when not provided by the connection key
    #[arg(long)]
    tun_addr: Option<String>,

    /// Route all OS traffic through the VPN tunnel; valid only in tun mode
    #[arg(long, action = ArgAction::SetTrue)]
    full_tunnel: bool,

    /// Local SOCKS5 host; valid only in socks5 mode
    #[arg(long)]
    local_socks5_host: Option<String>,

    /// Local SOCKS5 port; valid only in socks5 mode
    #[arg(long)]
    local_socks5_port: Option<u16>,

    /// Local SOCKS5 max concurrent client sessions; valid only in socks5 mode
    #[arg(long)]
    local_socks5_max_clients: Option<usize>,

    /// Local SOCKS5 max concurrent upstream connect attempts; valid only in socks5 mode
    #[arg(long)]
    local_socks5_max_concurrent_dials: Option<usize>,

    /// Log level: error, warn, info, debug, trace
    #[arg(long, value_enum)]
    log_level: Option<LogLevel>,

    /// JSON config file path
    #[arg(long)]
    config: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FileClientConfig {
    connection_key: Option<String>,
    #[serde(alias = "server")]
    server_addr: Option<String>,
    #[serde(alias = "server_key")]
    server_public_key: Option<String>,
    preshared_key: Option<String>,
    tun_name: Option<String>,
    tun_addr: Option<String>,
    full_tunnel: Option<bool>,
    mode: Option<RuntimeMode>,
    log_level: Option<LogLevel>,
    network_config: Option<ClientNetworkConfig>,
    local_socks5: Option<LocalSocks5Config>,
}

#[derive(Debug, Clone)]
struct ConnectionSettings {
    server_addr: String,
    server_public_key: [u8; 32],
    preshared_key: Option<[u8; 32]>,
    network_config: ClientNetworkConfig,
}

#[derive(Debug, Clone)]
struct RuntimeSettings {
    connection: ConnectionSettings,
    mode: RuntimeMode,
    tun_name: String,
    full_tunnel: bool,
    local_socks5: Option<LocalSocks5Config>,
}

#[tokio::main]
async fn main() {
    let args = ClientArgs::parse();
    let file_config = match load_file_config(args.config.as_deref()) {
        Ok(file_config) => file_config,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    };

    init_logging(&args, &file_config);

    let settings = match resolve_runtime_settings_with_file_config(&args, &file_config) {
        Ok(settings) => settings,
        Err(err) => {
            error!("{err}");
            std::process::exit(1);
        }
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
        info!("Received Ctrl+C, shutting down");
        shutdown_clone.store(true, Ordering::SeqCst);
    });

    info!("AIVPN Client v{}", env!("CARGO_PKG_VERSION"));
    info!("Mode: {}", settings.mode.as_str());
    info!("Connecting to server: {}", settings.connection.server_addr);
    if let Some(local_socks5) = &settings.local_socks5 {
        info!(
            "Local SOCKS5 listener: {} (max_clients {}, max_concurrent_dials {})",
            local_socks5.display_addr(),
            local_socks5.max_clients,
            local_socks5.max_concurrent_dials
        );
    }

    let local_socks5_runtime = Arc::new(LocalSocks5Runtime::new(
        settings
            .local_socks5
            .as_ref()
            .map(|cfg| cfg.max_concurrent_dials)
            .unwrap_or_else(|| LocalSocks5Config::default().max_concurrent_dials),
    ));
    let mut local_socks5_task = match settings.local_socks5.clone() {
        Some(config) => match spawn_local_socks5_server(config, local_socks5_runtime.clone()).await {
            Ok(task) => Some(task),
            Err(err) => {
                error!("Failed to start local SOCKS5 listener: {err}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    let mut backoff = INITIAL_RECONNECT_BACKOFF;
    let max_backoff = if settings.mode == RuntimeMode::Socks5 {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(60)
    };
    let mut reconnect_attempt: u32 = 0;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            info!("Shutdown requested, stopping client loop");
            break;
        }

        if let Some(handle) = take_finished_listener(&mut local_socks5_task) {
            report_listener_failure(handle).await;
        }

        let config = ClientConfig {
            server_addr: settings.connection.server_addr.clone(),
            server_public_key: settings.connection.server_public_key,
            preshared_key: settings.connection.preshared_key,
            initial_mask: webrtc_zoom_v3(),
            tun_config: TunnelConfig::from_network_config(
                settings.tun_name.clone(),
                settings.connection.network_config,
                settings.full_tunnel,
            ),
            mode: settings.mode.as_client_mode(),
            local_socks5_runtime: settings
                .local_socks5
                .as_ref()
                .map(|_| local_socks5_runtime.clone()),
            server_signing_pub: None,
        };

        let reconnect_delay = backoff;
        let advance_backoff = true;

        match AivpnClient::new(config) {
            Ok(mut client) => {
                info!(
                    "Client initialized successfully (TUN: {})",
                    settings.tun_name
                );

                let _ = fs::write("/var/run/aivpn/traffic.stats", "sent:0,received:0");
                let _ = fs::write("/tmp/aivpn-traffic.stats", "sent:0,received:0");

                match client.run(shutdown.clone()).await {
                    Ok(()) => break,
                    Err(err) => {
                        reconnect_attempt = reconnect_attempt.saturating_add(1);
                        warn!(
                            "Client run failed: {}. Reconnect attempt #{} in {}s",
                            err,
                            reconnect_attempt,
                            backoff.as_secs()
                        );
                    }
                }
            }
            Err(err) => {
                reconnect_attempt = reconnect_attempt.saturating_add(1);
                error!(
                    "Failed to create client: {}. Reconnect attempt #{} in {}s",
                    err,
                    reconnect_attempt,
                    backoff.as_secs()
                );
            }
        }

        if shutdown.load(Ordering::SeqCst) {
            info!("Shutdown requested after failure");
            break;
        }

        if !reconnect_delay.is_zero() {
            tokio::time::sleep(reconnect_delay).await;
        }
        if advance_backoff {
            backoff = std::cmp::min(backoff * 2, max_backoff);
        }
    }

    if let Some(task) = local_socks5_task.take() {
        task.abort();
    }
}

fn resolve_runtime_settings(args: &ClientArgs) -> Result<RuntimeSettings> {
    let file_config = load_file_config(args.config.as_deref())?;
    resolve_runtime_settings_with_file_config(args, &file_config)
}

fn resolve_runtime_settings_with_file_config(
    args: &ClientArgs,
    file_config: &FileClientConfig,
) -> Result<RuntimeSettings> {
    let mode = args.mode.or(file_config.mode).unwrap_or(RuntimeMode::Tun);
    let full_tunnel = args.full_tunnel || file_config.full_tunnel.unwrap_or(false);

    if mode == RuntimeMode::Socks5 && full_tunnel {
        return Err(Error::Session(
            "--full-tunnel cannot be used with --mode socks5".into(),
        ));
    }

    let tun_addr = args
        .tun_addr
        .clone()
        .or_else(|| file_config.tun_addr.clone())
        .unwrap_or_else(|| "10.0.0.2".to_string());
    let tun_name = args
        .tun_name
        .clone()
        .or_else(|| file_config.tun_name.clone())
        .unwrap_or_else(generate_tun_name);
    let local_socks5 = resolve_local_socks5_config(mode, args, &file_config)?;
    let connection = resolve_connection_settings(args, &file_config, &tun_addr)?;

    Ok(RuntimeSettings {
        connection,
        mode,
        tun_name,
        full_tunnel,
        local_socks5,
    })
}

fn init_logging(args: &ClientArgs, file_config: &FileClientConfig) {
    tracing_subscriber::fmt()
        .with_env_filter(resolve_log_filter(args, file_config))
        .init();
}

fn resolve_log_filter(
    args: &ClientArgs,
    file_config: &FileClientConfig,
) -> tracing_subscriber::EnvFilter {
    if let Some(level) = resolve_configured_log_level(args, file_config) {
        return tracing_subscriber::EnvFilter::new(level.as_filter());
    }

    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(LogLevel::Info.as_filter())
    })
}

fn resolve_configured_log_level(
    args: &ClientArgs,
    file_config: &FileClientConfig,
) -> Option<LogLevel> {
    args.log_level.or(file_config.log_level)
}

fn load_file_config(path: Option<&str>) -> Result<FileClientConfig> {
    let Some(path) = path else {
        return Ok(FileClientConfig::default());
    };

    let raw = fs::read_to_string(path).map_err(|err| {
        Error::Io(std::io::Error::new(
            err.kind(),
            format!("Failed to read config file {path}: {err}"),
        ))
    })?;

    serde_json::from_str(&raw).map_err(|err| {
        Error::Session(format!("Failed to parse config file {path}: {err}"))
    })
}

fn resolve_local_socks5_config(
    mode: RuntimeMode,
    args: &ClientArgs,
    file_config: &FileClientConfig,
) -> Result<Option<LocalSocks5Config>> {
    let cli_local_socks5 = args.local_socks5_host.is_some()
        || args.local_socks5_port.is_some()
        || args.local_socks5_max_clients.is_some()
        || args.local_socks5_max_concurrent_dials.is_some();

    match mode {
        RuntimeMode::Tun => {
            if cli_local_socks5 || file_config.local_socks5.is_some() {
                return Err(Error::Session(
                    "local_socks5 settings are only valid in socks5 mode".into(),
                ));
            }
            Ok(None)
        }
        RuntimeMode::Socks5 => {
            let mut config = file_config.local_socks5.clone().unwrap_or_default();
            if let Some(host) = &args.local_socks5_host {
                config.host = host.clone();
            }
            if let Some(port) = args.local_socks5_port {
                config.port = port;
            }
            if let Some(max_clients) = args.local_socks5_max_clients {
                config.max_clients = max_clients;
            }
            if let Some(max_concurrent_dials) = args.local_socks5_max_concurrent_dials {
                config.max_concurrent_dials = max_concurrent_dials;
            }
            config.validate()?;
            Ok(Some(config))
        }
    }
}

fn resolve_connection_settings(
    args: &ClientArgs,
    file_config: &FileClientConfig,
    fallback_tun_addr: &str,
) -> Result<ConnectionSettings> {
    if let Some(connection_key) = args.connection_key.as_deref() {
        return parse_connection_key(connection_key, fallback_tun_addr);
    }

    if args.server.is_some() || args.server_key.is_some() {
        return resolve_explicit_connection(args, file_config, fallback_tun_addr);
    }

    if let Some(connection_key) = file_config.connection_key.as_deref() {
        return parse_connection_key(connection_key, fallback_tun_addr);
    }

    resolve_explicit_connection(args, file_config, fallback_tun_addr)
}

fn resolve_explicit_connection(
    args: &ClientArgs,
    file_config: &FileClientConfig,
    fallback_tun_addr: &str,
) -> Result<ConnectionSettings> {
    let server_addr = args
        .server
        .clone()
        .or_else(|| file_config.server_addr.clone())
        .ok_or_else(|| {
            Error::Session(
                "Either --connection-key or --server + --server-key is required".into(),
            )
        })?;
    let server_key_b64 = args
        .server_key
        .clone()
        .or_else(|| file_config.server_public_key.clone())
        .ok_or_else(|| {
            Error::Session(
                "Either --connection-key or --server + --server-key is required".into(),
            )
        })?;
    let server_public_key = decode_base64_key("server public key", &server_key_b64)?;
    let preshared_key = file_config
        .preshared_key
        .as_deref()
        .map(|value| decode_base64_key("pre-shared key", value))
        .transpose()?;
    let network_config = file_config
        .network_config
        .unwrap_or_else(|| fallback_network_config(fallback_tun_addr));

    Ok(ConnectionSettings {
        server_addr,
        server_public_key,
        preshared_key,
        network_config,
    })
}

fn parse_connection_key(connection_key: &str, fallback_tun_addr: &str) -> Result<ConnectionSettings> {
    let payload = connection_key
        .trim()
        .strip_prefix("aivpn://")
        .unwrap_or(connection_key.trim());
    let json_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|err| Error::Session(format!("Invalid connection key: {err}")))?;
    let json: serde_json::Value = serde_json::from_slice(&json_bytes)
        .map_err(|err| Error::Session(format!("Malformed connection key JSON: {err}")))?;

    let server_addr = json["s"]
        .as_str()
        .ok_or_else(|| Error::Session("Connection key missing server address (\"s\")".into()))?
        .to_string();
    let server_key_b64 = json["k"]
        .as_str()
        .ok_or_else(|| Error::Session("Connection key missing server key (\"k\")".into()))?;
    let server_public_key = decode_base64_key("server public key", server_key_b64)?;
    let preshared_key = json["p"]
        .as_str()
        .map(|value| decode_base64_key("pre-shared key", value))
        .transpose()?;
    let network_config = json
        .get("n")
        .cloned()
        .and_then(|value| serde_json::from_value::<ClientNetworkConfig>(value).ok())
        .or_else(|| {
            json["i"].as_str().and_then(|ip| {
                ip.parse::<Ipv4Addr>().ok().map(|client_ip| ClientNetworkConfig {
                    client_ip,
                    server_vpn_ip: LEGACY_SERVER_VPN_IP,
                    prefix_len: 24,
                    mtu: DEFAULT_VPN_MTU,
                })
            })
        })
        .unwrap_or_else(|| fallback_network_config(fallback_tun_addr));

    Ok(ConnectionSettings {
        server_addr,
        server_public_key,
        preshared_key,
        network_config,
    })
}

fn decode_base64_key(label: &str, value: &str) -> Result<[u8; 32]> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|err| Error::Session(format!("Invalid {label}: {err}")))?;

    if decoded.len() != 32 {
        return Err(Error::Session(format!(
            "{label} must be 32 bytes after base64 decoding, got {}",
            decoded.len()
        )));
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&decoded);
    Ok(key)
}

fn fallback_network_config(tun_addr: &str) -> ClientNetworkConfig {
    let client_ip = tun_addr.parse::<Ipv4Addr>().unwrap_or_else(|_| {
        error!("Invalid TUN address '{tun_addr}': expected IPv4 address");
        std::process::exit(1);
    });

    ClientNetworkConfig {
        client_ip,
        server_vpn_ip: LEGACY_SERVER_VPN_IP,
        prefix_len: 24,
        mtu: DEFAULT_VPN_MTU,
    }
}

fn generate_tun_name() -> String {
    format!("tun{:04x}", rand::thread_rng().gen::<u16>())
}

fn take_finished_listener(
    task: &mut Option<JoinHandle<Result<()>>>,
) -> Option<JoinHandle<Result<()>>> {
    if task.as_ref().is_some_and(|handle| handle.is_finished()) {
        task.take()
    } else {
        None
    }
}

async fn report_listener_failure(task: JoinHandle<Result<()>>) -> ! {
    match task.await {
        Ok(Ok(())) => {
            error!("Local SOCKS5 listener exited unexpectedly");
        }
        Ok(Err(err)) => {
            error!("Local SOCKS5 listener failed: {err}");
        }
        Err(err) => {
            error!("Local SOCKS5 listener task crashed: {err}");
        }
    }
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socks5_mode_rejects_full_tunnel() {
        let args = ClientArgs {
            server: Some("1.2.3.4:443".into()),
            server_key: Some(base64::engine::general_purpose::STANDARD.encode([7u8; 32])),
            connection_key: None,
            mode: Some(RuntimeMode::Socks5),
            tun_name: None,
            tun_addr: None,
            full_tunnel: true,
            local_socks5_host: None,
            local_socks5_port: None,
            local_socks5_max_clients: None,
            local_socks5_max_concurrent_dials: None,
            log_level: None,
            config: None,
        };

        let err = resolve_runtime_settings(&args).unwrap_err();
        assert!(err.to_string().contains("--full-tunnel"));
    }

    #[test]
    fn socks5_mode_defaults_local_listener() {
        let args = ClientArgs {
            server: Some("1.2.3.4:443".into()),
            server_key: Some(base64::engine::general_purpose::STANDARD.encode([7u8; 32])),
            connection_key: None,
            mode: Some(RuntimeMode::Socks5),
            tun_name: None,
            tun_addr: None,
            full_tunnel: false,
            local_socks5_host: None,
            local_socks5_port: None,
            local_socks5_max_clients: None,
            local_socks5_max_concurrent_dials: None,
            log_level: None,
            config: None,
        };

        let settings = resolve_runtime_settings(&args).unwrap();
        let local_socks5 = settings.local_socks5.unwrap();
        assert_eq!(local_socks5.host, "127.0.0.1");
        assert_eq!(local_socks5.port, 1080);
        assert_eq!(local_socks5.max_clients, 256);
        assert_eq!(local_socks5.max_concurrent_dials, 128);
    }

    #[test]
    fn cli_log_level_overrides_config_log_level() {
        let args = ClientArgs {
            server: None,
            server_key: None,
            connection_key: Some("aivpn://placeholder".into()),
            mode: None,
            tun_name: None,
            tun_addr: None,
            full_tunnel: false,
            local_socks5_host: None,
            local_socks5_port: None,
            local_socks5_max_clients: None,
            local_socks5_max_concurrent_dials: None,
            log_level: Some(LogLevel::Warn),
            config: None,
        };
        let file_config = FileClientConfig {
            log_level: Some(LogLevel::Debug),
            ..Default::default()
        };

        assert_eq!(
            resolve_configured_log_level(&args, &file_config),
            Some(LogLevel::Warn)
        );
    }

    #[test]
    fn config_log_level_used_when_cli_is_absent() {
        let args = ClientArgs {
            server: None,
            server_key: None,
            connection_key: Some("aivpn://placeholder".into()),
            mode: None,
            tun_name: None,
            tun_addr: None,
            full_tunnel: false,
            local_socks5_host: None,
            local_socks5_port: None,
            local_socks5_max_clients: None,
            local_socks5_max_concurrent_dials: None,
            log_level: None,
            config: None,
        };
        let file_config = FileClientConfig {
            log_level: Some(LogLevel::Error),
            ..Default::default()
        };

        assert_eq!(
            resolve_configured_log_level(&args, &file_config),
            Some(LogLevel::Error)
        );
    }

}
