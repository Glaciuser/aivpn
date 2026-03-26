//! AIVPN Client Implementation
//! 
//! Client with:
//! - TUN device for packet capture
//! - Mimicry Engine for traffic shaping
//! - Key exchange and session management

pub mod client;
pub mod mimicry;
pub mod tunnel;

pub use client::AivpnClient;
pub use mimicry::MimicryEngine;
pub use tunnel::Tunnel;
