pub mod bencode;
pub mod bitmap;
pub mod error;
pub mod magnet;
pub mod manager;
pub mod metadata;
pub mod storage;

pub use bitmap::PieceBitmap;
pub use error::TorrentError;
pub use magnet::{parse_magnet_uri, ParsedMagnet};
pub use manager::{TorrentAddOptions, TorrentManager, TorrentManagerConfig, TorrentStatsReport};
pub use metadata::{
    build_v1_multi_file_torrent, build_v1_single_file_torrent, parse_torrent_bytes,
    TorrentFileSpec, TorrentMetadata, TorrentVersion,
};
pub use storage::{ConfinedStorageFactory, ConfinedTorrentStorage};

pub fn torrent_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
