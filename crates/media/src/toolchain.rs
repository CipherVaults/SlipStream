use std::{
    env,
    ffi::OsStr,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use sha2::{Digest, Sha256};
use slipstream_protocol::AppSettings;
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

use crate::{args::validate_local_path, MediaError};

#[derive(Debug, Clone, Default)]
pub struct ToolchainConfig {
    pub ytdlp_path: Option<PathBuf>,
    pub ffmpeg_path: Option<PathBuf>,
    pub ffprobe_path: Option<PathBuf>,
    pub expected_ytdlp_sha256: Option<String>,
    pub expected_ffmpeg_sha256: Option<String>,
    pub expected_ffprobe_sha256: Option<String>,
}

impl From<&AppSettings> for ToolchainConfig {
    fn from(settings: &AppSettings) -> Self {
        Self {
            ytdlp_path: settings.ytdlp_path.as_ref().map(PathBuf::from),
            ffmpeg_path: settings.ffmpeg_path.as_ref().map(PathBuf::from),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedBinary {
    pub path: PathBuf,
    pub version: String,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct MediaToolchain {
    ytdlp: Option<ResolvedBinary>,
    ffmpeg: ResolvedBinary,
    ffprobe: Option<ResolvedBinary>,
}

impl MediaToolchain {
    pub async fn discover(config: impl Into<ToolchainConfig>) -> Result<Self, MediaError> {
        let config = config.into();
        let ytdlp = resolve(
            YTDLP,
            config.ytdlp_path.as_deref(),
            None,
            config.expected_ytdlp_sha256.as_deref(),
        )
        .await?
        .ok_or(MediaError::BinaryNotFound { tool: YTDLP.name })?;
        let mut tools = Self::discover_ffmpeg_only(config).await?;
        tools.ytdlp = Some(ytdlp);
        Ok(tools)
    }

    pub async fn discover_ffmpeg_only(
        config: impl Into<ToolchainConfig>,
    ) -> Result<Self, MediaError> {
        let config = config.into();
        let ffmpeg = resolve(
            FFMPEG,
            config.ffmpeg_path.as_deref(),
            None,
            config.expected_ffmpeg_sha256.as_deref(),
        )
        .await?
        .ok_or(MediaError::BinaryNotFound { tool: FFMPEG.name })?;
        let sibling = config
            .ffmpeg_path
            .as_ref()
            .and_then(|_| ffmpeg.path.parent());
        let ffprobe = resolve(
            FFPROBE,
            config.ffprobe_path.as_deref(),
            sibling,
            config.expected_ffprobe_sha256.as_deref(),
        )
        .await?;
        if ffprobe.is_none() && config.expected_ffprobe_sha256.is_some() {
            return Err(MediaError::BinaryNotFound { tool: FFPROBE.name });
        }
        Ok(Self {
            ytdlp: None,
            ffmpeg,
            ffprobe,
        })
    }

    pub fn ytdlp(&self) -> Option<&ResolvedBinary> {
        self.ytdlp.as_ref()
    }

    pub fn ffmpeg(&self) -> &ResolvedBinary {
        &self.ffmpeg
    }

    pub fn ffprobe(&self) -> Option<&ResolvedBinary> {
        self.ffprobe.as_ref()
    }

    pub(crate) fn require_ytdlp(&self) -> Result<&ResolvedBinary, MediaError> {
        self.ytdlp()
            .ok_or(MediaError::MissingTool { tool: YTDLP.name })
    }
}

#[derive(Clone, Copy)]
struct Tool {
    name: &'static str,
    version_flag: &'static str,
}

impl Tool {
    fn executable(self) -> String {
        format!("{}{}", self.name, env::consts::EXE_SUFFIX)
    }
}

const YTDLP: Tool = Tool {
    name: "yt-dlp",
    version_flag: "--version",
};
const FFMPEG: Tool = Tool {
    name: "ffmpeg",
    version_flag: "-version",
};
const FFPROBE: Tool = Tool {
    name: "ffprobe",
    version_flag: "-version",
};

async fn resolve(
    tool: Tool,
    configured: Option<&Path>,
    sibling: Option<&Path>,
    expected_sha256: Option<&str>,
) -> Result<Option<ResolvedBinary>, MediaError> {
    if let Some(expected) = expected_sha256 {
        validate_checksum(tool.name, expected)?;
    }
    let candidate = match configured {
        Some(path) => path.to_owned(),
        None => {
            let exe = tool.executable();
            let sibling = sibling
                .map(|dir| dir.join(&exe))
                .filter(|path| path.is_file());
            let Some(path) = sibling.or_else(|| search_path(&exe)) else {
                return Ok(None);
            };
            path
        }
    };
    let path = canonical_binary(tool.name, &candidate).await?;
    let sha256 = verify_sha256(tool.name, &path, expected_sha256).await?;
    let mut cmd = command(&path, tool.name);
    cmd.arg(tool.version_flag);
    let output = capture(cmd, &path, tool.name, Duration::from_secs(8), 64 * 1024).await?;
    let version = String::from_utf8_lossy(&output)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or(MediaError::InvalidToolOutput {
            tool: tool.name,
            kind: "version",
        })?
        .to_owned();
    Ok(Some(ResolvedBinary {
        path,
        version,
        sha256,
    }))
}

fn search_path(executable: &str) -> Option<PathBuf> {
    env::var_os("PATH")
        .iter()
        .flat_map(env::split_paths)
        // Relative and empty entries would search the working directory.
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(executable))
        .find(|path| path.is_file())
}

async fn canonical_binary(tool: &'static str, path: &Path) -> Result<PathBuf, MediaError> {
    let invalid = |path: &Path, reason| MediaError::InvalidBinary {
        tool,
        path: path.to_owned(),
        reason,
    };
    if path.as_os_str().is_empty() {
        return Err(invalid(path, "empty path"));
    }
    if is_shell_script(path) {
        return Err(invalid(path, "batch scripts are disallowed"));
    }
    let canonical = fs::canonicalize(path)
        .await
        .map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => invalid(path, "path does not exist"),
            _ => MediaError::io("canonicalize binary", path, err),
        })?;
    if is_shell_script(&canonical) {
        return Err(invalid(&canonical, "batch scripts are disallowed"));
    }
    if cfg!(windows) && !has_extension(&canonical, "exe") {
        return Err(invalid(
            &canonical,
            "a Windows executable must have the .exe extension",
        ));
    }
    if validate_local_path(&canonical).is_err() {
        return Err(invalid(&canonical, "expected a safe local executable path"));
    }
    let metadata = fs::metadata(&canonical)
        .await
        .map_err(|err| MediaError::io("inspect binary", &canonical, err))?;
    if !metadata.is_file() {
        return Err(invalid(&canonical, "expected a regular file"));
    }
    Ok(canonical)
}

fn is_shell_script(path: &Path) -> bool {
    has_extension(path, "bat") || has_extension(path, "cmd")
}

pub(crate) fn has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case(expected))
}

fn validate_checksum(tool: &'static str, expected: &str) -> Result<(), MediaError> {
    if expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(MediaError::InvalidChecksum { tool })
    }
}

async fn verify_sha256(
    tool: &'static str,
    path: &Path,
    expected: Option<&str>,
) -> Result<String, MediaError> {
    let mut file = fs::File::open(path)
        .await
        .map_err(|err| MediaError::io("open binary", path, err))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let size = file
            .read(&mut buffer)
            .await
            .map_err(|err| MediaError::io("hash binary", path, err))?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    let actual = hex::encode(hash.finalize());
    if let Some(expected) = expected {
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(MediaError::ChecksumMismatch {
                tool,
                path: path.to_owned(),
                expected: expected.into(),
                actual,
            });
        }
    }
    Ok(actual)
}

pub(crate) fn command(path: &Path, tool: &str) -> Command {
    let mut cmd = Command::new(path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if tool == YTDLP.name {
        cmd.env("YTDLP_NO_PLUGINS", "1");
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

pub(crate) async fn capture(
    mut cmd: Command,
    path: &Path,
    tool: &'static str,
    timeout: Duration,
    stdout_limit: usize,
) -> Result<Vec<u8>, MediaError> {
    tokio::time::timeout(timeout, async {
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let (output, stderr, status) = tokio::try_join!(
            async move {
                let mut output = Vec::new();
                stdout
                    .take(stdout_limit as u64 + 1)
                    .read_to_end(&mut output)
                    .await?;
                Ok(output)
            },
            read_stderr(stderr),
            child.wait()
        )?;
        Ok::<_, io::Error>((output, stderr, status))
    })
    .await
    .map_err(|_| MediaError::Timeout { tool, timeout })?
    .map_err(|err| MediaError::io("run sidecar", path, err))
    .and_then(|(output, stderr, status)| {
        if output.len() > stdout_limit {
            return Err(MediaError::InvalidToolOutput {
                tool,
                kind: "stdout",
            });
        }
        if !status.success() {
            return Err(MediaError::ToolFailed {
                tool,
                status,
                stderr,
            });
        }
        Ok(output)
    })
}

pub(crate) async fn read_stderr(mut pipe: impl AsyncRead + Unpin) -> io::Result<String> {
    const LIMIT: usize = 8192;
    let mut tail = Vec::new();
    let mut buffer = [0_u8; LIMIT];
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            return Ok(String::from_utf8_lossy(&tail).trim().to_owned());
        }
        let overflow = (tail.len() + count).saturating_sub(LIMIT);
        tail.drain(..overflow);
        tail.extend_from_slice(&buffer[..count]);
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = env::temp_dir().join(format!("slipstream-toolchain-{}", Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn sha256_pins_are_checked_before_execution() {
        let dir = TempDir::new();
        let path = dir.0.join("ffmpeg.exe");
        fs::write(&path, b"abc").await.unwrap();
        let known = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(
            verify_sha256("ffmpeg", &path, Some(&known.to_ascii_uppercase()))
                .await
                .unwrap(),
            known
        );
        let mut config = ToolchainConfig {
            ffmpeg_path: Some(path),
            expected_ffmpeg_sha256: Some("0".repeat(64)),
            ..ToolchainConfig::default()
        };
        assert!(
            matches!(MediaToolchain::discover_ffmpeg_only(config.clone()).await,
            Err(MediaError::ChecksumMismatch { tool: "ffmpeg", actual, .. }) if actual == known)
        );
        config.expected_ffmpeg_sha256 = Some("g".repeat(64));
        assert!(matches!(
            MediaToolchain::discover_ffmpeg_only(config).await,
            Err(MediaError::InvalidChecksum { tool: "ffmpeg" })
        ));
    }

    #[tokio::test]
    async fn rejects_non_binaries_and_batch_files() {
        let dir = TempDir::new();
        let directory = dir.0.join("directory.exe");
        fs::create_dir(&directory).await.unwrap();
        let missing = dir.0.join("missing.exe");
        let batch = dir.0.join("script.CmD");
        fs::write(&batch, "").await.unwrap();
        for path in [Path::new(""), &directory, &missing, &batch] {
            assert!(matches!(
                canonical_binary("ffmpeg", path).await,
                Err(MediaError::InvalidBinary { .. })
            ));
        }
    }
}
