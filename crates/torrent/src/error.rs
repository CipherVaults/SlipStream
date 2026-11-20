use slipstream_protocol::{PathSecurityError, UrlSecurityError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TorrentError {
    #[error(transparent)]
    PathSecurity(#[from] PathSecurityError),
    #[error(transparent)]
    UrlSecurity(#[from] UrlSecurityError),
    #[error("invalid bencode: {0}")]
    Bencode(String),
    #[error("invalid torrent metadata: {0}")]
    Metadata(String),
    #[error("torrent not found: {0}")]
    NotFound(String),
    #[error("invalid torrent state: {0}")]
    InvalidState(String),
    #[error("unsupported torrent operation: {0}")]
    Unsupported(String),
    #[error("BitTorrent session: {0}")]
    Session(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub(crate) fn session_error(error: anyhow::Error) -> TorrentError {
    match error.downcast::<TorrentError>() {
        Ok(error) => error,
        Err(error) => TorrentError::Session(format!("{error:#}")),
    }
}
