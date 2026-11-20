use std::{fs, path::Path};

use slipstream_protocol::validate_http_url;

use crate::{args::validate_local_path, MediaError};

/// Writes the Internet zone marker on Windows. The URL is validated on all platforms.
pub fn write_mark_of_the_web(path: &Path, source_url: &str) -> Result<(), MediaError> {
    validate_local_path(path)?;
    let url = sanitize_motw_url(source_url)?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| MediaError::io("inspect media for Mark-of-the-Web", path, err))?;
    if !is_regular_file(&metadata) {
        return Err(MediaError::argument(
            "Mark-of-the-Web file",
            path.to_string_lossy(),
        ));
    }

    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        fs::write(
            std::path::PathBuf::from(stream),
            format!("[ZoneTransfer]\r\nZoneId=3\r\nHostUrl={url}\r\n"),
        )
        .map_err(|err| MediaError::io("write Mark-of-the-Web", path, err))?;
    }
    #[cfg(not(windows))]
    let _ = url;

    Ok(())
}

fn sanitize_motw_url(source_url: &str) -> Result<String, MediaError> {
    let mut url = validate_http_url(source_url)?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Ok(url.into())
}

pub(crate) fn is_regular_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !is_reparse_point(metadata)
}

pub(crate) fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    metadata.is_symlink()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_credentials_from_motw_host_url() {
        assert_eq!(
            sanitize_motw_url("https://alice:secret@example.com/media/clip.mp4?token=1").unwrap(),
            "https://example.com/media/clip.mp4?token=1"
        );
        assert_eq!(
            sanitize_motw_url("http://user@cdn.example.org/video.webm").unwrap(),
            "http://cdn.example.org/video.webm"
        );
    }
}
