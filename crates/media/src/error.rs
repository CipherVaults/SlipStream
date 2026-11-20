use std::{io, path::PathBuf, process::ExitStatus, time::Duration};

use slipstream_protocol::{PathSecurityError, UrlSecurityError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error(transparent)]
    Url(#[from] UrlSecurityError),
    #[error(transparent)]
    Path(#[from] PathSecurityError),
    #[error("invalid {kind}: {value:?}")]
    InvalidArgument { kind: &'static str, value: String },
    #[error("{tool} was not found on PATH")]
    BinaryNotFound { tool: &'static str },
    #[error("invalid {tool} binary at {path:?}: {reason}")]
    InvalidBinary {
        tool: &'static str,
        path: PathBuf,
        reason: &'static str,
    },
    #[error("the pinned SHA-256 for {tool} must contain exactly 64 hexadecimal digits")]
    InvalidChecksum { tool: &'static str },
    #[error("SHA-256 mismatch for {tool} at {path:?}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        tool: &'static str,
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("{tool} is required for this operation")]
    MissingTool { tool: &'static str },
    #[error("{tool} timed out after {timeout:?}")]
    Timeout {
        tool: &'static str,
        timeout: Duration,
    },
    #[error("{tool} failed with {status}: {stderr}")]
    ToolFailed {
        tool: &'static str,
        status: ExitStatus,
        stderr: String,
    },
    #[error("{tool} returned an invalid or oversized {kind}")]
    InvalidToolOutput {
        tool: &'static str,
        kind: &'static str,
    },
    #[error("{action} {path:?}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid yt-dlp probe JSON: {0}")]
    ProbeJson(#[from] serde_json::Error),
    #[error("no media files were produced")]
    NoMedia,
    #[error("destination already exists: {0:?}")]
    DestinationExists(PathBuf),
    #[error("publishing failed after committing {published:?}: {source}")]
    PartialPublish {
        published: Vec<PathBuf>,
        #[source]
        source: Box<MediaError>,
    },
}

impl MediaError {
    pub(crate) fn argument(kind: &'static str, value: impl Into<String>) -> Self {
        Self::InvalidArgument {
            kind,
            value: value.into(),
        }
    }

    pub(crate) fn io(action: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}
