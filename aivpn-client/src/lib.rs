//! AIVPN Client Implementation
//! 
//! Client with:
//! - TUN device for packet capture
//! - Mimicry Engine for traffic shaping
//! - Key exchange and session management

pub mod client;
pub mod local_socks;
pub mod mimicry;
pub mod netns;
pub mod tunnel;

pub use client::AivpnClient;
pub use mimicry::MimicryEngine;
pub use tunnel::Tunnel;
