//! AIVPN Client Binary - Full Implementation

use aivpn_client::AivpnClient;
use aivpn_client::client::ClientConfig;
use aivpn_client::tunnel::TunnelConfig;
use aivpn_common::mask::preset_masks::webrtc_zoom_v3;
use clap::Parser;
use tracing::{info, error};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// AIVPN Client - Censorship-resistant VPN client
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct ClientArgs {
    /// Server address (e.g., 1.2.3.4:443)
    #[arg(short, long, required = true)]
    pub server: String,

    /// Server public key (base64, 32 bytes)
    #[arg(long, required = true)]
    pub server_key: String,

    /// TUN device name (random if not specified)
    #[arg(long)]
    pub tun_name: Option<String>,

    /// TUN device address
    #[arg(long, default_value = "10.0.0.2")]
    pub tun_addr: String,

    /// Route all traffic through VPN tunnel
    #[arg(long, default_value_t = false)]
    pub full_tunnel: bool,

    /// Config file path (JSON)
    #[arg(short, long)]
    pub config: Option<String>,
}

// Global shutdown flag
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[tokio::main]
async fn main() {
    // Initialize logging first
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
        )
        .init();

    // Setup Ctrl+C handler in a separate task
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.expect("Failed to setup signal handler");
        info!("Received Ctrl+C, shutting down...");
        shutdown_clone.store(true, Ordering::SeqCst);
        SHUTDOWN.store(true, Ordering::SeqCst);
    });
    
    // Parse arguments
    let args = ClientArgs::parse();
    
    info!("AIVPN Client v{}", env!("CARGO_PKG_VERSION"));
    info!("Connecting to server: {}", args.server);
    
    // Parse server key
    let server_key_decoded = match base64::decode(&args.server_key) {
        Ok(key) => key,
        Err(e) => {
            error!("Invalid server key: {}", e);
            std::process::exit(1);
        }
    };
    
    let mut server_public_key = [0u8; 32];
    if server_key_decoded.len() != 32 {
        error!("Server key must be 32 bytes, got {}", server_key_decoded.len());
        std::process::exit(1);
    }
    server_public_key.copy_from_slice(&server_key_decoded);
    
    // Create config
    let config = ClientConfig {
        server_addr: args.server,
        server_public_key,
        preshared_key: None,
        initial_mask: webrtc_zoom_v3(),
        server_signing_pub: None,
        tun_config: TunnelConfig {
            tun_name: args.tun_name.unwrap_or_else(|| {
                use rand::Rng;
                format!("tun{:04x}", rand::thread_rng().gen::<u16>())
            }),
            tun_addr: args.tun_addr,
            tun_netmask: "255.255.255.0".to_string(),
            mtu: 1280,
            full_tunnel: args.full_tunnel,
        },
    };
    
    // Create and run client
    match AivpnClient::new(config) {
        Ok(mut client) => {
            info!("Client initialized successfully");
            
            // Run client in same task (don't spawn)
            if let Err(e) = client.run().await {
                error!("Client error: {}", e);
                std::process::exit(1);
            }
        }
        Err(e) => {
            error!("Failed to create client: {}", e);
            std::process::exit(1);
        }
    }
}
