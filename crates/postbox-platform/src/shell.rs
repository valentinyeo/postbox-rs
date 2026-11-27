use crate::PlatformError;
use std::path::{Path, PathBuf};

/// Returns `None` on cancellation or on platforms without a native picker.
pub fn pick_attachment_file() -> Result<Option<PathBuf>, PlatformError> {
    #[cfg(windows)]
    {
        crate::win::shell::pick_attachment_file()
    }
    #[cfg(not(windows))]
    {
        Ok(None)
    }
}

pub fn open_external_video(path: &Path) -> Result<(), PlatformError> {
    let raw = path.to_str().ok_or(PlatformError::InvalidPath)?;
    let filename = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let extension = filename.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    if raw.contains('\0')
        || filename.contains(':')
        || ![
            "mp4", "m4v", "mov", "webm", "mkv", "avi", "wmv", "mpeg", "mpg", "3gp",
        ]
        .iter()
        .any(|allowed| extension.eq_ignore_ascii_case(allowed))
    {
        return Err(PlatformError::InvalidPath);
    }
    #[cfg(windows)]
    {
        crate::win::shell::open_external_file(path)
    }
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_video_rejects_executables_and_alternate_streams() {
        for path in [
            "clip.mp4.exe",
            "clip.mp4:evil.mp4",
            "clip.mp4\\evil.exe",
            "clip.mp4\0.mp4",
        ] {
            assert!(open_external_video(Path::new(path)).is_err(), "{path:?}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn portable_shell_is_a_no_op() {
        assert!(pick_attachment_file().unwrap().is_none());
        assert!(open_external_video(Path::new("clip.MP4")).is_ok());
    }
}
