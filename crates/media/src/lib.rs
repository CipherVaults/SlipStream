pub mod args;
pub mod error;
pub mod motw;
pub mod pipeline;
pub mod probe;
pub mod toolchain;

pub use args::{BuildFfmpegArgs, BuildYtDlpArgs};
pub use error::MediaError;
pub use pipeline::{
    DownloadOptions, DownloadOutcome, DownloadProgress, FfmpegProgress, LocalMediaOptions,
    MediaMetadata, MediaPipeline, MediaProgressEvent, SubtitleInput,
};
pub use probe::{
    audio_preset_recommendations, parse_ytdlp_probe_json, pick_format_option, probe_url,
    probe_url_with_binary, quality_summary_tags, AudioPresetRecommendation, ProbeOptions,
};
pub use toolchain::{discover_ytdlp_only, MediaToolchain, ResolvedBinary, ToolchainConfig};

pub fn media_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
