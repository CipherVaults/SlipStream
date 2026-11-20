pub mod args;
pub mod error;
pub mod motw;
pub mod pipeline;
pub mod probe;
pub mod toolchain;

pub use args::{BuildFfmpegArgs, BuildYtDlpArgs};
pub use error::MediaError;
pub use pipeline::{
    DownloadOptions, DownloadProgress, FfmpegProgress, LocalMediaOptions, MediaMetadata,
    MediaPipeline, MediaProgressEvent, SubtitleInput,
};
pub use probe::{
    audio_preset_recommendations, parse_ytdlp_probe_json, pick_format_option, probe_url,
    quality_summary_tags, AudioPresetRecommendation, ProbeOptions,
};
pub use toolchain::{MediaToolchain, ResolvedBinary, ToolchainConfig};

pub fn media_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
