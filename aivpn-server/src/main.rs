//! AIVPN Server Binary

use aivpn_server::{AivpnServer, ServerArgs};
use aivpn_server::gateway::GatewayConfig;
use aivpn_server::neural::NeuralConfig;
use aivpn_common::crypto;
use tracing::{info, error};
use clap::Parser;

#[tokio::main]
async fn main() {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
        )
        .init();

    info!("AIVPN Server v{}", env!("CARGO_PKG_VERSION"));
    info!("Starting server...");

    // Parse arguments
    let args = ServerArgs::parse_from(std::env::args());

    info!("Listening on: {}", args.listen);

    // Load server private key from file if provided (HIGH-11)
    let server_private_key = if let Some(ref key_file) = args.key_file {
        let key_data = std::fs::read(key_file)
            .unwrap_or_else(|e| {
                error!("Failed to read key file '{}': {}", key_file, e);
                std::process::exit(1);
            });
        if key_data.len() != 32 {
            error!("Key file must be exactly 32 bytes, got {}", key_data.len());
            std::process::exit(1);
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_data);
        info!("Loaded server key from file");
        // Log public key so the client can be configured
        let kp = crypto::KeyPair::from_private_key(key);
        let pub_bytes = kp.public_key_bytes();
        info!("Server public key (hex): {}", pub_bytes.iter().map(|b| format!("{:02x}", b)).collect::<String>());
        key
    } else {
        info!("No --key-file provided, server key will be ephemeral");
        [0u8; 32]
    };

    // Generate random TUN name if not specified (MED-1: avoids fingerprinting)
    let tun_name = args.tun_name.unwrap_or_else(|| {
        use rand::Rng;
        format!("tun{:04x}", rand::thread_rng().gen::<u16>())
    });

    // Create config
    let config = GatewayConfig {
        listen_addr: args.listen,
        tun_name,
        tun_addr: "10.0.0.1".to_string(),
        tun_netmask: "255.255.255.0".to_string(),
        server_private_key,
        signing_key: [0u8; 64],
        enable_nat: true,
        enable_neural: true,
        neural_config: NeuralConfig::default(),
    };

    // Create and run server
    match AivpnServer::new(config) {
        Ok(mut server) => {
            info!("Server initialized successfully");
            if let Err(e) = server.run().await {
                error!("Server error: {}", e);
                std::process::exit(1);
            }
        }
        Err(e) => {
            error!("Failed to create server: {}", e);
            std::process::exit(1);
        }
    }
}
