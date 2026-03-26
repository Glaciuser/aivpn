//! Tunnel Module - Real TUN Device Integration
//! 
//! Handles TUN device creation, packet capture, and routing

use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, debug, error};

use aivpn_common::error::{Error, Result};

/// Tunnel configuration
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    pub tun_name: String,
    pub tun_addr: String,
    pub tun_netmask: String,
    pub mtu: u16,
    /// Route all traffic through VPN (full tunnel mode)
    pub full_tunnel: bool,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        use rand::Rng;
        Self {
            tun_name: format!("tun{:04x}", rand::thread_rng().gen::<u16>()),
            tun_addr: "10.0.0.1".to_string(),
            tun_netmask: "255.255.255.0".to_string(),
            mtu: 1280,
            full_tunnel: false,
        }
    }
}

/// TUN Tunnel for packet capture
pub struct Tunnel {
    config: TunnelConfig,
    reader: Option<tun::DeviceReader>,
    writer: Option<tun::DeviceWriter>,
    /// Saved default gateway for full-tunnel restore
    #[cfg(target_os = "macos")]
    saved_default_gw: Option<String>,
    /// Server IP for bypass route cleanup
    #[cfg(target_os = "macos")]
    server_ip: Option<String>,
}

impl Tunnel {
    pub fn new(config: TunnelConfig) -> Self {
        Self {
            config,
            reader: None,
            writer: None,
            #[cfg(target_os = "macos")]
            saved_default_gw: None,
            #[cfg(target_os = "macos")]
            server_ip: None,
        }
    }
    
    /// Create TUN device
    pub fn create(&mut self) -> Result<()> {
        let mut config_builder = tun::Configuration::default();
        
        config_builder
            .address(&self.config.tun_addr)
            .netmask(&self.config.tun_netmask)
            .mtu(self.config.mtu)
            .up();
        
        #[cfg(target_os = "linux")]
        {
            config_builder.name(&self.config.tun_name);
            config_builder.platform_config(|config| {
                config.ensure_root_privileges(true);
            });
        }
        
        let dev = tun::create_as_async(&config_builder)
            .map_err(|e| Error::Io(io::Error::new(io::ErrorKind::Other, e.to_string())))?;
        
        // Get actual device name before split (on macOS, name is assigned by kernel as utunN)
        if let Ok(actual_name) = tun::AbstractDevice::tun_name(&*dev) {
            self.config.tun_name = actual_name;
        }
        
        // Split into independent reader/writer — no Mutex needed for concurrent I/O
        let (writer, reader) = dev.split()
            .map_err(Error::Io)?;
        self.reader = Some(reader);
        self.writer = Some(writer);
        
        info!(
            "Created TUN device: {} ({}/{})",
            self.config.tun_name,
            self.config.tun_addr,
            self.config.tun_netmask
        );
        
        // On macOS, explicitly configure the point-to-point tunnel
        #[cfg(target_os = "macos")]
        self.configure_macos()?;
        
        Ok(())
    }
    
    /// Configure TUN device on macOS (ifconfig + route)
    #[cfg(target_os = "macos")]
    fn configure_macos(&self) -> Result<()> {
        use std::process::Command;
        
        let tun_name = &self.config.tun_name;
        let tun_addr = &self.config.tun_addr;
        let peer_addr = "10.0.0.1"; // Server-side TUN address
        
        // Set point-to-point addresses with explicit netmask
        let status = Command::new("ifconfig")
            .args([tun_name, "inet", tun_addr, peer_addr, "netmask", "255.255.255.0", "mtu", &self.config.mtu.to_string(), "up"])
            .status()
            .map_err(|e| Error::Io(io::Error::new(io::ErrorKind::Other, 
                format!("Failed to run ifconfig: {}", e))))?;
        
        if !status.success() {
            error!("ifconfig failed with status: {}", status);
        } else {
            info!("Configured {} with {} -> {} (netmask 255.255.255.0)", tun_name, tun_addr, peer_addr);
        }
        
        // First: delete any stale routes to prevent conflicts
        let _ = Command::new("route")
            .args(["-n", "delete", "-host", peer_addr])
            .status();
        let _ = Command::new("route")
            .args(["-n", "delete", "-net", "10.0.0.0/24"])
            .status();
        
        // Add host route for the peer (10.0.0.1)
        let status = Command::new("route")
            .args(["-n", "add", "-host", peer_addr, "-interface", tun_name])
            .status()
            .map_err(|e| Error::Io(io::Error::new(io::ErrorKind::Other, 
                format!("Failed to add host route: {}", e))))?;
        
        if !status.success() {
            error!("route add -host {} failed: {}", peer_addr, status);
        } else {
            info!("Added host route {} via {}", peer_addr, tun_name);
        }
        
        // Add subnet route for 10.0.0.0/24
        let status = Command::new("route")
            .args(["-n", "add", "-net", "10.0.0.0/24", "-interface", tun_name])
            .status()
            .map_err(|e| Error::Io(io::Error::new(io::ErrorKind::Other, 
                format!("Failed to add route: {}", e))))?;
        
        if !status.success() {
            debug!("route add -net 10.0.0.0/24 failed (may already exist): {}", status);
        } else {
            info!("Added route 10.0.0.0/24 via {}", tun_name);
        }
        
        Ok(())
    }
    
    /// Set VPN server IP (call before enable_full_tunnel)
    pub fn set_server_ip(&mut self, server_ip: String) {
        #[cfg(target_os = "macos")]
        { self.server_ip = Some(server_ip); }
        #[cfg(not(target_os = "macos"))]
        { let _ = server_ip; }
    }
    
    /// Enable full-tunnel mode: route all traffic through VPN
    #[cfg(target_os = "macos")]
    pub fn enable_full_tunnel(&mut self) -> Result<()> {
        use std::process::Command;
        
        let tun_name = &self.config.tun_name;
        let peer_addr = "10.0.0.1";
        
        // 1. Get current default gateway
        let output = Command::new("route")
            .args(["-n", "get", "default"])
            .output()
            .map_err(|e| Error::Io(io::Error::new(io::ErrorKind::Other,
                format!("Failed to get default route: {}", e))))?;
        
        let stdout = String::from_utf8_lossy(&output.stdout);
        let default_gw = stdout.lines()
            .find(|l| l.trim().starts_with("gateway:"))
            .and_then(|l| l.split(':').nth(1))
            .map(|s| s.trim().to_string());
        
        let gw = match default_gw {
            Some(g) => g,
            None => {
                error!("Could not determine default gateway");
                return Err(Error::Io(io::Error::new(io::ErrorKind::Other,
                    "Could not determine default gateway")));
            }
        };
        
        info!("Current default gateway: {}", gw);
        self.saved_default_gw = Some(gw.clone());
        
        // 2. Add bypass route for VPN server IP via original gateway
        if let Some(ref server_ip) = self.server_ip {
            let _ = Command::new("route")
                .args(["-n", "delete", "-host", server_ip])
                .status();
            let status = Command::new("route")
                .args(["-n", "add", "-host", server_ip, &gw])
                .status()
                .map_err(|e| Error::Io(io::Error::new(io::ErrorKind::Other,
                    format!("Failed to add server bypass route: {}", e))))?;
            if status.success() {
                info!("Added bypass route: {} via {}", server_ip, gw);
            } else {
                error!("Failed to add bypass route for {}", server_ip);
            }
        }
        
        // 3. Route all traffic through TUN using 0/1 + 128/1 trick
        //    These are more specific than 0.0.0.0/0 so they take priority
        for net in ["0.0.0.0/1", "128.0.0.0/1"] {
            let _ = Command::new("route")
                .args(["-n", "delete", "-net", net])
                .status();
            let status = Command::new("route")
                .args(["-n", "add", "-net", net, "-interface", tun_name])
                .status()
                .map_err(|e| Error::Io(io::Error::new(io::ErrorKind::Other,
                    format!("Failed to add full-tunnel route {}: {}", net, e))))?;
            if status.success() {
                info!("Added full-tunnel route: {} via {}", net, tun_name);
            } else {
                error!("Failed to add full-tunnel route {}", net);
            }
        }
        
        info!("Full tunnel mode enabled — all traffic routed through VPN");
        Ok(())
    }
    
    /// Disable full-tunnel mode: restore original routing
    #[cfg(target_os = "macos")]
    fn disable_full_tunnel(&mut self) {
        use std::process::Command;
        
        // Remove catch-all routes
        for net in ["0.0.0.0/1", "128.0.0.0/1"] {
            let _ = Command::new("route")
                .args(["-n", "delete", "-net", net])
                .status();
        }
        
        // Remove server bypass route
        if let Some(ref server_ip) = self.server_ip {
            let _ = Command::new("route")
                .args(["-n", "delete", "-host", server_ip])
                .status();
        }
        
        info!("Full tunnel routes removed");
    }
    
    /// Take the TUN reader (moves ownership to caller, e.g. spawned task)
    pub fn take_reader(&mut self) -> Option<tun::DeviceReader> {
        self.reader.take()
    }

    /// Write packet to TUN asynchronously
    pub async fn write_packet_async(&mut self, packet: &[u8]) -> Result<usize> {
        let writer = self.writer.as_mut()
            .ok_or_else(|| Error::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "TUN writer not available",
            )))?;
        
        writer.write_all(packet).await?;
        writer.flush().await?;
        
        debug!("Wrote {} bytes to TUN", packet.len());
        Ok(packet.len())
    }
    
    /// Get TUN device name
    pub fn name(&self) -> &str {
        &self.config.tun_name
    }
    
    /// Get TUN config
    pub fn config(&self) -> &TunnelConfig {
        &self.config
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if self.config.full_tunnel && self.saved_default_gw.is_some() {
            self.disable_full_tunnel();
        }
        if self.writer.is_some() || self.reader.is_some() {
            info!("Closing TUN device: {}", self.config.tun_name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_tunnel_config() {
        let config = TunnelConfig::default();
        assert!(config.tun_name.starts_with("tun"), "TUN name should start with 'tun'");
        assert_eq!(config.tun_addr, "10.0.0.1");
        assert_eq!(config.mtu, 1280);
    }
}
