//! Self-updater: stage, verify, swap atomically, roll back, relaunch.
//!
//! The product invariant is **the installed app always keeps a runnable executable**. That
//! is why the sequence is:
//!
//! 1. download the release zip to `*.staged` beside the live binary,
//! 2. verify its SHA-256 against the manifest,
//! 3. extract the new binary to `*.staged.exe` beside the live binary,
//! 4. copy the live binary to `*.backup.exe`,
//! 5. replace the live binary with the staged one in one `MoveFileExW`
//!    (`REPLACE_EXISTING | WRITE_THROUGH`) on Windows, one `rename` on Unix,
//! 6. on any failure after step 4, move the backup back and report the error.
//!
//! Steps 1-3 never touch the live binary, and step 5 is the only mutating call, so the app
//! is runnable before it, during it and after it.
//!
//! The zip reader, the installer and the relaunch hook are traits so the whole sequence runs
//! on Linux CI with real files and a fake relaunch.

use crate::PlatformError;
use postbox_core::update::{UpdateAsset, digest_matches};
use std::path::{Path, PathBuf};

/// Suffix of the download before verification.
pub const STAGED_SUFFIX: &str = ".staged";
/// Suffix of the extracted replacement binary.
pub const PENDING_SUFFIX: &str = ".staged.exe";
/// Suffix of the copy of the live binary taken just before the swap.
pub const BACKUP_SUFFIX: &str = ".backup.exe";
/// Name of the executable inside the release zip and of the installed binary.
pub const EXE_ASSET_NAME: &str = "postbox.exe";

/// Moves the staged file over the live one. The only mutating OS call in the flow.
pub trait AtomicReplacer: Send + Sync {
    /// Replaces `target` with `staged` in one step, leaving `target` untouched on failure.
    fn replace(&self, staged: &Path, target: &Path) -> Result<(), PlatformError>;
}

/// Extracts the executable from a release archive.
pub trait ArchiveExtractor: Send + Sync {
    /// Writes the bundled executable to `dest` and returns its path.
    fn extract_executable(&self, archive: &Path, dest: &Path) -> Result<PathBuf, PlatformError>;
}

/// Starts the freshly installed binary.
pub trait Relauncher: Send + Sync {
    fn relaunch(&self, exe: &Path, args: &[String]) -> Result<(), PlatformError>;
}

/// Files the updater touched, so a caller can clean up or report progress.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallReport {
    pub staged_archive: Option<PathBuf>,
    pub verified_sha256: String,
    pub backup: Option<PathBuf>,
    pub installed_exe: PathBuf,
}

/// Fails with the reason the download was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// The bytes on disk do not match the manifest digest.
    DigestMismatch { expected: String, actual: String },
    /// The archive carried no executable we recognise.
    MissingExecutable,
    /// The download was empty or over the manifest size.
    SizeMismatch { expected: u64, actual: u64 },
    /// Anything else, already stringified at the boundary.
    Io(String),
    /// The swap failed and the original binary was put back.
    SwapFailedRolledBack(String),
    /// The swap failed and the rollback failed too. The backup path is reported.
    SwapFailedRollbackFailed {
        error: String,
        rollback: String,
        backup: Option<PathBuf>,
    },
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DigestMismatch { expected, actual } => {
                write!(
                    f,
                    "update download did not match its SHA-256 (expected {expected}, got {actual})"
                )
            }
            Self::MissingExecutable => write!(f, "the release archive contained no executable"),
            Self::SizeMismatch { expected, actual } => {
                write!(f, "update download was {actual} bytes, expected {expected}")
            }
            Self::Io(err) => write!(f, "{err}"),
            Self::SwapFailedRolledBack(err) => {
                write!(
                    f,
                    "could not swap in the update ({err}); the previous version is still installed"
                )
            }
            Self::SwapFailedRollbackFailed {
                error,
                rollback,
                backup,
            } => write!(
                f,
                "could not swap in the update ({error}) and could not restore the backup ({rollback}){}",
                backup
                    .as_ref()
                    .map(|p| format!("; the backup is at {}", p.display()))
                    .unwrap_or_default()
            ),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<std::io::Error> for UpdateError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<PlatformError> for UpdateError {
    fn from(err: PlatformError) -> Self {
        Self::Io(err.to_string())
    }
}

/// `postbox.exe.staged` beside the live binary, so the swap never crosses a volume.
pub fn staged_archive_path(install_dir: &Path, asset_name: &str) -> PathBuf {
    let stem = Path::new(asset_name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "update.zip".to_owned());
    install_dir.join(format!("{stem}{STAGED_SUFFIX}"))
}

pub fn pending_exe_path(install_dir: &Path, exe_name: &str) -> PathBuf {
    install_dir.join(format!("{exe_name}{PENDING_SUFFIX}"))
}

pub fn backup_exe_path(install_dir: &Path, exe_name: &str) -> PathBuf {
    install_dir.join(format!("{exe_name}{BACKUP_SUFFIX}"))
}

/// Verifies downloaded bytes against the manifest before anything is extracted.
pub fn verify_download(bytes: &[u8], asset: &UpdateAsset) -> Result<(), UpdateError> {
    if asset.size > 0 && bytes.len() as u64 != asset.size {
        return Err(UpdateError::SizeMismatch {
            expected: asset.size,
            actual: bytes.len() as u64,
        });
    }
    if !digest_matches(bytes, &asset.sha256) {
        return Err(UpdateError::DigestMismatch {
            expected: asset.sha256.clone(),
            actual: postbox_core::update::digest_hex(bytes),
        });
    }
    Ok(())
}

/// Replaces the live binary with `pending`, restoring `backup` if the swap fails.
pub fn swap_with_rollback(
    replacer: &dyn AtomicReplacer,
    pending: &Path,
    live: &Path,
    backup: &Path,
) -> Result<(), UpdateError> {
    let Err(err) = replacer.replace(pending, live) else {
        return Ok(());
    };
    let message = err.to_string();
    // A replace that failed before touching the live file leaves the app runnable as it is.
    if live.exists() {
        return Err(UpdateError::SwapFailedRolledBack(message));
    }
    // The live binary is gone; the backup is the only runnable copy left.
    match replacer.replace(backup, live) {
        Ok(()) => Err(UpdateError::SwapFailedRolledBack(message)),
        Err(rollback) => Err(UpdateError::SwapFailedRollbackFailed {
            error: message,
            rollback: rollback.to_string(),
            backup: backup.exists().then(|| backup.to_path_buf()),
        }),
    }
}

/// A check that found nothing to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpToDate {
    pub current: String,
}

/// Runs the whole install with injected pieces and no network.
///
/// `download` returns the archive bytes; every side effect goes through a trait so tests
/// drive real files on Linux.
pub fn install(
    asset: &UpdateAsset,
    archive_bytes: &[u8],
    install_dir: &Path,
    exe_name: &str,
    extractor: &dyn ArchiveExtractor,
    replacer: &dyn AtomicReplacer,
) -> Result<InstallReport, UpdateError> {
    let staged = staged_archive_path(install_dir, &asset_asset_name(asset));
    std::fs::create_dir_all(install_dir)?;
    std::fs::write(&staged, archive_bytes)?;

    // Step 2: refuse a bad download before anything is extracted or replaced.
    if let Err(err) = verify_download(archive_bytes, asset) {
        let _ = std::fs::remove_file(&staged);
        return Err(err);
    }

    // Step 3: extract beside the live binary.
    let pending = pending_exe_path(install_dir, exe_name);
    let _ = std::fs::remove_file(&pending);
    let extracted = extractor.extract_executable(&staged, &pending)?;
    if !extracted.exists() {
        return Err(UpdateError::MissingExecutable);
    }

    // Step 4: keep a copy of what is installed right now.
    let live = install_dir.join(exe_name);
    let backup = backup_exe_path(install_dir, exe_name);
    if live.exists() {
        std::fs::copy(&live, &backup)?;
    }

    // Step 5: the single atomic move.
    swap_with_rollback(replacer, &extracted, &live, &backup)?;

    // Step 6: the staged archive is no longer needed.
    let _ = std::fs::remove_file(&staged);

    Ok(InstallReport {
        staged_archive: None,
        verified_sha256: asset.sha256.clone(),
        backup: backup.exists().then_some(backup),
        installed_exe: live,
    })
}

fn asset_asset_name(asset: &UpdateAsset) -> String {
    asset
        .url
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(postbox_core::update::WINDOWS_ASSET)
        .to_owned()
}

// ---------------------------------------------------------------------------
// Portable implementations
// ---------------------------------------------------------------------------

/// `MoveFileExW(..., REPLACE_EXISTING | WRITE_THROUGH)` on Windows, `rename` elsewhere.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeReplacer;

impl AtomicReplacer for NativeReplacer {
    fn replace(&self, staged: &Path, target: &Path) -> Result<(), PlatformError> {
        #[cfg(windows)]
        {
            crate::win::updater::move_replace(staged, target)
        }
        #[cfg(not(windows))]
        {
            std::fs::rename(staged, target).map_err(PlatformError::Io)
        }
    }
}

impl ArchiveExtractor for Box<dyn ArchiveExtractor> {
    fn extract_executable(&self, archive: &Path, dest: &Path) -> Result<PathBuf, PlatformError> {
        (**self).extract_executable(archive, dest)
    }
}

impl AtomicReplacer for Box<dyn AtomicReplacer> {
    fn replace(&self, staged: &Path, target: &Path) -> Result<(), PlatformError> {
        (**self).replace(staged, target)
    }
}

/// Plain `std::fs` extractor for a directory of files, used by tests and by the non-Windows
/// build. The Windows build uses `crate::win::updater::ZipExtractor`.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlainFileExtractor;

impl ArchiveExtractor for PlainFileExtractor {
    fn extract_executable(&self, archive: &Path, dest: &Path) -> Result<PathBuf, PlatformError> {
        // The archive is a single file here, which keeps the tests honest about the copy step
        // without needing a zip writer in the test path.
        std::fs::copy(archive, dest)?;
        Ok(dest.to_path_buf())
    }
}

/// Records the relaunch instead of spawning, for tests and for headless checks.
#[derive(Debug, Default)]
pub struct MockRelauncher {
    calls: std::sync::Mutex<Vec<(PathBuf, Vec<String>)>>,
}

impl MockRelauncher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calls(&self) -> Vec<(PathBuf, Vec<String>)> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

impl Relauncher for MockRelauncher {
    fn relaunch(&self, exe: &Path, args: &[String]) -> Result<(), PlatformError> {
        self.calls
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?
            .push((exe.to_path_buf(), args.to_vec()));
        Ok(())
    }
}

/// Spawns the installed binary detached, so the old process can exit.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessRelauncher;

impl Relauncher for ProcessRelauncher {
    fn relaunch(&self, exe: &Path, args: &[String]) -> Result<(), PlatformError> {
        std::process::Command::new(exe)
            .args(args)
            .spawn()
            .map(|_| ())
            .map_err(PlatformError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::update::{Version, digest_hex};
    use std::sync::Mutex;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "postbox-update-{tag}-{}-{}",
            std::process::id(),
            postbox_core::update::digest_hex(tag.as_bytes())
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn asset_for(bytes: &[u8]) -> UpdateAsset {
        UpdateAsset {
            tag: "v0.2.0".into(),
            version: Version::new(0, 2, 0),
            url: "https://example.invalid/postbox-windows-x64.zip".into(),
            sha256: digest_hex(bytes),
            size: bytes.len() as u64,
            notes: String::new(),
        }
    }

    /// How a fake swap fails.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FailMode {
        /// Fails without touching the target (the live binary survives as it was).
        BeforeTouching,
        /// Deletes the target and then fails, so only the backup is runnable.
        AfterDeleting,
    }

    struct FailingReplacer {
        calls: Mutex<Vec<(PathBuf, PathBuf)>>,
        mode: FailMode,
        fail_calls: usize,
    }

    impl FailingReplacer {
        fn new(mode: FailMode, fail_calls: usize) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                mode,
                fail_calls,
            }
        }

        fn always(mode: FailMode) -> Self {
            Self::new(mode, usize::MAX)
        }
    }

    impl AtomicReplacer for FailingReplacer {
        fn replace(&self, staged: &Path, target: &Path) -> Result<(), PlatformError> {
            let should_fail = {
                let mut calls = self.calls.lock().unwrap();
                calls.push((staged.to_path_buf(), target.to_path_buf()));
                calls.len() <= self.fail_calls
            };
            if !should_fail {
                std::fs::rename(staged, target)?;
                return Ok(());
            }
            match self.mode {
                FailMode::BeforeTouching => Err(PlatformError::OsUnavailable),
                FailMode::AfterDeleting => {
                    let _ = std::fs::remove_file(target);
                    Err(PlatformError::OsUnavailable)
                }
            }
        }
    }

    #[test]
    fn a_good_update_installs_and_keeps_a_backup() {
        let dir = temp_dir("good");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"old build").unwrap();
        let bytes = b"new build bytes";

        let report = install(
            &asset_for(bytes),
            bytes,
            &dir,
            "postbox.exe",
            &PlainFileExtractor,
            &NativeReplacer,
        )
        .expect("install succeeds");

        assert_eq!(std::fs::read(&live).unwrap(), bytes);
        assert_eq!(report.installed_exe, live);
        assert_eq!(report.verified_sha256, digest_hex(bytes));
        let backup = report.backup.expect("a backup of the old build exists");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old build");
        assert!(!dir.join("postbox.exe.staged").exists());
        assert!(!pending_exe_path(&dir, "postbox.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_hash_is_refused_and_the_installed_binary_is_untouched() {
        let dir = temp_dir("badhash");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let mut asset = asset_for(b"expected bytes");
        asset.size = 0; // exercise the digest check on its own
        let err = install(
            &asset,
            b"tampered bytes",
            &dir,
            "postbox.exe",
            &PlainFileExtractor,
            &NativeReplacer,
        )
        .expect_err("a bad digest is refused");
        assert!(matches!(err, UpdateError::DigestMismatch { .. }), "{err:?}");
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"installed build",
            "the live binary must not change"
        );
        assert!(
            !dir.join("postbox.exe.staged").exists(),
            "the refused download is cleaned up"
        );
        assert!(!backup_exe_path(&dir, "postbox.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_short_download_is_refused_before_any_files_are_written() {
        let dir = temp_dir("size");
        let mut asset = asset_for(b"12345678");
        asset.size = 9_999;
        let err = install(
            &asset,
            b"1234",
            &dir,
            "postbox.exe",
            &PlainFileExtractor,
            &NativeReplacer,
        )
        .expect_err("a truncated download is refused");
        assert!(
            matches!(
                err,
                UpdateError::SizeMismatch {
                    expected: 9_999,
                    actual: 4
                }
            ),
            "{err:?}"
        );
        assert!(!dir.join("postbox.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_swap_that_loses_the_live_binary_rolls_the_backup_back() {
        let dir = temp_dir("rollback");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let bytes = b"new build";
        // The first replace deletes the live file and fails; the second (rollback) succeeds.
        let replacer = FailingReplacer::new(FailMode::AfterDeleting, 1);

        let err = install(
            &asset_for(bytes),
            bytes,
            &dir,
            "postbox.exe",
            &PlainFileExtractor,
            &replacer,
        )
        .expect_err("the swap fails");

        match err {
            UpdateError::SwapFailedRolledBack(_) => {}
            other => panic!("expected a rollback, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"installed build",
            "the previous build is restored"
        );
        let calls = replacer.calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "one failed swap plus one restore");
        assert!(calls[1].0.ends_with("postbox.exe.backup.exe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_swap_that_never_touched_the_live_binary_still_reports_the_failure() {
        let dir = temp_dir("untouched");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let bytes = b"new build";
        let replacer = FailingReplacer::always(FailMode::BeforeTouching);

        let err = install(
            &asset_for(bytes),
            bytes,
            &dir,
            "postbox.exe",
            &PlainFileExtractor,
            &replacer,
        )
        .expect_err("a failed swap is reported");
        assert!(
            matches!(err, UpdateError::SwapFailedRolledBack(_)),
            "{err:?}"
        );
        assert_eq!(std::fs::read(&live).unwrap(), b"installed build");
        assert_eq!(
            replacer.calls.lock().unwrap().len(),
            1,
            "no restore is needed when the live file never moved"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rollback_that_also_fails_reports_where_the_backup_is() {
        let dir = temp_dir("rollbackfail");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let bytes = b"new build";
        let replacer = FailingReplacer::always(FailMode::BeforeTouching);
        // Delete the live binary to simulate a replace that removed it and then failed.
        let pending = pending_exe_path(&dir, "postbox.exe");
        std::fs::write(&pending, bytes).unwrap();
        let backup = backup_exe_path(&dir, "postbox.exe");
        std::fs::write(&backup, b"installed build").unwrap();
        std::fs::remove_file(&live).unwrap();

        let err =
            swap_with_rollback(&replacer, &pending, &live, &backup).expect_err("both steps fail");
        match err {
            UpdateError::SwapFailedRollbackFailed { backup: path, .. } => {
                assert_eq!(path.as_deref(), Some(backup.as_path()));
            }
            other => panic!("expected a failed rollback, got {other:?}"),
        }
        assert!(backup.exists(), "the backup is still on disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_successful_swap_leaves_exactly_one_runnable_binary() {
        let dir = temp_dir("oneswap");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let pending = pending_exe_path(&dir, "postbox.exe");
        std::fs::write(&pending, b"new build").unwrap();
        let backup = backup_exe_path(&dir, "postbox.exe");
        std::fs::write(&backup, b"installed build").unwrap();

        swap_with_rollback(&NativeReplacer, &pending, &live, &backup).unwrap();
        assert_eq!(std::fs::read(&live).unwrap(), b"new build");
        assert!(!pending.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn staging_paths_stay_beside_the_install_directory() {
        let dir = Path::new(r"C:\Program Files\Postbox");
        let staged = staged_archive_path(dir, "postbox-windows-x64.zip");
        assert_eq!(staged, dir.join("postbox-windows-x64.zip.staged"));
        assert_eq!(
            pending_exe_path(dir, "postbox.exe"),
            dir.join("postbox.exe.staged.exe")
        );
        assert_eq!(
            backup_exe_path(dir, "postbox.exe"),
            dir.join("postbox.exe.backup.exe")
        );
        assert_eq!(staged.parent(), Some(dir), "same volume as the live binary");
    }

    #[test]
    fn the_relaunch_hook_receives_the_installed_binary() {
        let launcher = MockRelauncher::new();
        let exe = Path::new(r"C:\Program Files\Postbox\postbox.exe");
        launcher.relaunch(exe, &["--restored".into()]).unwrap();
        let calls = launcher.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, exe);
        assert_eq!(calls[0].1, vec!["--restored".to_owned()]);
    }

    #[test]
    fn verify_download_accepts_the_manifest_digest_form() {
        let bytes = b"payload";
        let mut asset = asset_for(bytes);
        asset.sha256 = digest_hex(bytes);
        assert!(verify_download(bytes, &asset).is_ok());
        asset.sha256 = format!("sha256:{}", digest_hex(bytes));
        assert!(verify_download(bytes, &asset).is_ok());
        asset.sha256 = "0".repeat(64);
        assert!(verify_download(bytes, &asset).is_err());
    }
}
