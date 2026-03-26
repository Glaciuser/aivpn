//! AIVPN Error Types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Invalid packet: {0}")]
    InvalidPacket(&'static str),

    #[error("Crypto error: {0}")]
    Crypto(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Session error: {0}")]
    Session(String),

    #[error("Mask error: {0}")]
    Mask(String),

    #[error("Channel error: {0}")]
    Channel(String),
}

impl From<chacha20poly1305::Error> for Error {
    fn from(err: chacha20poly1305::Error) -> Self {
        Error::Crypto(err.to_string())
    }
}

impl From<rmp_serde::encode::Error> for Error {
    fn from(err: rmp_serde::encode::Error) -> Self {
        Error::Serialization(err.to_string())
    }
}

impl From<rmp_serde::decode::Error> for Error {
    fn from(err: rmp_serde::decode::Error) -> Self {
        Error::Serialization(err.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Serialization(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
