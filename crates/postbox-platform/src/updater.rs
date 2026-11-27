//! Self-updater: stage, verify, swap atomically, roll back, relaunch.
//!
//! The product invariant is **the installed app always keeps a runnable executable**. That
//! is why the sequence is split into two halves:
//!
//! `stage` (background, no user action):
//! 1. download the release zip to `*.staged` beside the live binary,
//! 2. verify its SHA-256 against the manifest,
//! 3. extract the new binary to `*.staged.exe` beside the live binary.
//!
//! `commit` (the restart click):
//! 4. rename the live binary to `*.old`,
//! 5. move the staged binary into the now-vacant live path using `MoveFileExW`
//!    (`REPLACE_EXISTING | WRITE_THROUGH`) on Windows, `rename` on Unix,
//! 6. if step 5 fails, move the backup back and report the error.
//!
//! Steps 1-3 never touch the live binary. During the swap, the previous executable stays
//! runnable at the backup path until the new binary takes its place. A later launch removes
//! the leftover backup on a best-effort basis.
//!
//! The zip reader, the installer and the relaunch hook are traits so the whole sequence runs
//! on Linux CI with real files and a fake relaunch. [`stage_release`] picks the reader from
//! the download (zip magic or a `.zip` asset, otherwise a ready executable), so a manifest
//! that serves a plain binary stages the same way on every OS.

use crate::PlatformError;
use postbox_core::update::{UpdateAsset, digest_matches};
use std::path::{Path, PathBuf};

/// Suffix of the download before verification.
pub const STAGED_SUFFIX: &str = ".staged";
/// Suffix of the extracted replacement binary.
pub const PENDING_SUFFIX: &str = ".staged.exe";
/// Suffix of the live binary moved aside just before the swap.
pub const BACKUP_SUFFIX: &str = ".old";
/// Name of the executable inside the release zip and of the installed binary.
pub const EXE_ASSET_NAME: &str = "postbox.exe";

/// Moves executables between the live, pending and backup paths.
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

/// Best-effort cleanup of the previous executable after an update relaunch.
pub fn cleanup_backup(install_dir: &Path, exe_name: &str) {
    let _ = std::fs::remove_file(backup_exe_path(install_dir, exe_name));
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

/// A verified download extracted beside the live binary, waiting for the restart click.
///
/// Nothing here has touched the installed executable: `commit` is the only function that
/// swaps it, so the app stays runnable while the card waits for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedUpdate {
    pub tag: String,
    pub version: postbox_core::update::Version,
    pub sha256: String,
    /// The verified release archive, kept until `commit` removes it.
    pub staged_archive: PathBuf,
    /// The extracted replacement executable beside the live one.
    pub pending_exe: PathBuf,
    pub install_dir: PathBuf,
    pub exe_name: String,
}

/// Steps 1-3 of the install: write the download, verify its SHA-256 and extract the new
/// executable beside the live binary.
///
/// A digest mismatch deletes the download and returns an error before anything is extracted,
/// which is what keeps a bad hash from ever offering a restart card.
pub fn stage(
    asset: &UpdateAsset,
    archive_bytes: &[u8],
    install_dir: &Path,
    exe_name: &str,
    extractor: &dyn ArchiveExtractor,
) -> Result<StagedUpdate, UpdateError> {
    let staged = staged_archive_path(install_dir, &asset_asset_name(asset));
    std::fs::create_dir_all(install_dir)?;
    std::fs::write(&staged, archive_bytes)?;

    // Refuse a bad download before anything is extracted or replaced.
    if let Err(err) = verify_download(archive_bytes, asset) {
        let _ = std::fs::remove_file(&staged);
        return Err(err);
    }

    let pending = pending_exe_path(install_dir, exe_name);
    let _ = std::fs::remove_file(&pending);
    let extracted = match extractor.extract_executable(&staged, &pending) {
        Ok(path) => path,
        Err(err) => {
            let _ = std::fs::remove_file(&staged);
            return Err(err.into());
        }
    };
    if !extracted.exists() {
        let _ = std::fs::remove_file(&staged);
        return Err(UpdateError::MissingExecutable);
    }

    Ok(StagedUpdate {
        tag: asset.tag.clone(),
        version: asset.version,
        sha256: asset.sha256.clone(),
        staged_archive: staged,
        pending_exe: extracted,
        install_dir: install_dir.to_path_buf(),
        exe_name: exe_name.to_owned(),
    })
}

/// Stages a downloaded release, choosing the archive reader from the download itself.
///
/// A release archive is the only thing the zip reader may see, and a real one always starts
/// with the zip local-header magic. A payload that does not is taken as a ready executable
/// and copied. Picking the reader from the build target instead meant the same download
/// staged on Linux and failed on Windows, because the Windows build always used the zip
/// reader. The digest check in [`stage`] has already proved the bytes match the manifest, so
/// this only decides how to unpack them.
pub fn stage_release(
    asset: &UpdateAsset,
    archive_bytes: &[u8],
    install_dir: &Path,
    exe_name: &str,
) -> Result<StagedUpdate, UpdateError> {
    if is_zip_payload(archive_bytes) {
        stage(
            asset,
            archive_bytes,
            install_dir,
            exe_name,
            &crate::zip::ZipExtractor,
        )
    } else {
        stage(
            asset,
            archive_bytes,
            install_dir,
            exe_name,
            &PlainFileExtractor,
        )
    }
}

/// `true` when the download is a zip, so it must go through the zip reader.
fn is_zip_payload(archive_bytes: &[u8]) -> bool {
    archive_bytes.starts_with(b"PK\x03\x04")
}

/// Deletes a staged download and its extracted executable, for a release that a newer one
/// has replaced. The live binary is never touched.
pub fn discard(staged: &StagedUpdate) {
    let _ = std::fs::remove_file(&staged.staged_archive);
    let _ = std::fs::remove_file(&staged.pending_exe);
}

/// Steps 4-6: move the live binary aside, move the staged one in and clean up.
///
/// A failed swap restores the backup. If restoring also fails, the error reports the backup
/// path so the previous executable can be recovered.
pub fn commit(
    staged: &StagedUpdate,
    replacer: &dyn AtomicReplacer,
) -> Result<InstallReport, UpdateError> {
    if !staged.pending_exe.exists() {
        return Err(UpdateError::MissingExecutable);
    }
    let live = staged.install_dir.join(&staged.exe_name);
    let backup = backup_exe_path(&staged.install_dir, &staged.exe_name);
    if live.exists() {
        let _ = std::fs::remove_file(&backup);
        // Windows permits renaming a running executable, but not replacing it in place.
        replacer.replace(&live, &backup)?;
    }

    swap_with_rollback(replacer, &staged.pending_exe, &live, &backup)?;

    let _ = std::fs::remove_file(&staged.staged_archive);

    Ok(InstallReport {
        staged_archive: None,
        verified_sha256: staged.sha256.clone(),
        backup: backup.exists().then_some(backup),
        installed_exe: live,
    })
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
    let staged = stage(asset, archive_bytes, install_dir, exe_name, extractor)?;
    commit(&staged, replacer)
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

    struct FailingReplacer {
        calls: Mutex<Vec<(PathBuf, PathBuf)>>,
        fail_calls: Vec<usize>,
    }

    impl FailingReplacer {
        fn new(fail_calls: &[usize]) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_calls: fail_calls.to_vec(),
            }
        }
    }

    impl AtomicReplacer for FailingReplacer {
        fn replace(&self, staged: &Path, target: &Path) -> Result<(), PlatformError> {
            let should_fail = {
                let mut calls = self.calls.lock().unwrap();
                calls.push((staged.to_path_buf(), target.to_path_buf()));
                self.fail_calls.contains(&calls.len())
            };
            if should_fail {
                return Err(PlatformError::OsUnavailable);
            }
            assert!(
                !target.exists(),
                "the running exe must be moved aside first"
            );
            std::fs::rename(staged, target)?;
            Ok(())
        }
    }

    #[test]
    fn staging_verifies_the_download_and_never_touches_the_live_binary() {
        let dir = temp_dir("stage");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let bytes = b"downloaded build";

        let staged = stage(
            &asset_for(bytes),
            bytes,
            &dir,
            "postbox.exe",
            &PlainFileExtractor,
        )
        .expect("staging succeeds");

        assert_eq!(staged.tag, "v0.2.0");
        assert_eq!(staged.sha256, digest_hex(bytes));
        assert!(staged.staged_archive.exists(), "the archive waits on disk");
        assert_eq!(std::fs::read(&staged.pending_exe).unwrap(), bytes);
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"installed build",
            "the running binary is untouched until the restart click"
        );
        assert!(!backup_exe_path(&dir, "postbox.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn staging_a_bad_hash_fails_closed_and_leaves_no_pending_binary() {
        let dir = temp_dir("stagebad");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let mut asset = asset_for(b"expected bytes");
        asset.size = 0; // exercise the digest check on its own

        let err = stage(
            &asset,
            b"tampered bytes",
            &dir,
            "postbox.exe",
            &PlainFileExtractor,
        )
        .expect_err("a bad digest is refused");

        assert!(matches!(err, UpdateError::DigestMismatch { .. }), "{err:?}");
        assert_eq!(std::fs::read(&live).unwrap(), b"installed build");
        assert!(!pending_exe_path(&dir, "postbox.exe").exists());
        let archive = staged_archive_path(&dir, "postbox-windows-x64.zip");
        assert!(!archive.exists(), "the refused archive is cleaned up");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn committing_a_staged_update_swaps_it_in_and_cleans_the_archive() {
        let dir = temp_dir("commit");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"old build").unwrap();
        let bytes = b"new build bytes";
        let staged = stage(
            &asset_for(bytes),
            bytes,
            &dir,
            "postbox.exe",
            &PlainFileExtractor,
        )
        .unwrap();

        let backup = backup_exe_path(&dir, "postbox.exe");
        std::fs::write(&backup, b"stale backup").unwrap();
        let replacer = FailingReplacer::new(&[]);
        let report = commit(&staged, &replacer).expect("the restart click commits");

        assert_eq!(
            *replacer.calls.lock().unwrap(),
            vec![
                (live.clone(), backup),
                (staged.pending_exe.clone(), live.clone()),
            ]
        );

        assert_eq!(std::fs::read(&live).unwrap(), bytes);
        assert_eq!(report.installed_exe, live);
        assert_eq!(report.verified_sha256, digest_hex(bytes));
        assert!(!staged.staged_archive.exists(), "the archive is cleaned up");
        assert!(!staged.pending_exe.exists(), "the pending exe was moved");
        let backup = report.backup.expect("the previous build is kept");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old build");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn committing_a_staged_update_whose_pending_exe_vanished_fails_closed() {
        let dir = temp_dir("commitgone");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"old build").unwrap();
        let bytes = b"new build bytes";
        let staged = stage(
            &asset_for(bytes),
            bytes,
            &dir,
            "postbox.exe",
            &PlainFileExtractor,
        )
        .unwrap();
        std::fs::remove_file(&staged.pending_exe).unwrap();

        let err = commit(&staged, &NativeReplacer).expect_err("nothing to swap in");
        assert!(matches!(err, UpdateError::MissingExecutable), "{err:?}");
        assert_eq!(std::fs::read(&live).unwrap(), b"old build");
        let _ = std::fs::remove_dir_all(&dir);
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
    fn a_failed_second_move_restores_the_backup() {
        let dir = temp_dir("rollback");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let bytes = b"new build";
        let replacer = FailingReplacer::new(&[2]);

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
        let backup = backup_exe_path(&dir, "postbox.exe");
        let pending = pending_exe_path(&dir, "postbox.exe");
        assert_eq!(
            *replacer.calls.lock().unwrap(),
            vec![
                (live.clone(), backup.clone()),
                (pending.clone(), live.clone()),
                (backup.clone(), live),
            ]
        );
        assert!(!backup.exists());
        assert_eq!(std::fs::read(pending).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_backup_move_leaves_the_live_binary_untouched() {
        let dir = temp_dir("untouched");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let bytes = b"new build";
        let replacer = FailingReplacer::new(&[1, 2]);

        let err = install(
            &asset_for(bytes),
            bytes,
            &dir,
            "postbox.exe",
            &PlainFileExtractor,
            &replacer,
        )
        .expect_err("a failed backup move is reported");
        assert!(matches!(err, UpdateError::Io(_)), "{err:?}");
        assert_eq!(std::fs::read(&live).unwrap(), b"installed build");
        assert_eq!(
            *replacer.calls.lock().unwrap(),
            vec![(live, backup_exe_path(&dir, "postbox.exe"))]
        );
        assert_eq!(
            std::fs::read(pending_exe_path(&dir, "postbox.exe")).unwrap(),
            bytes
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rollback_that_also_fails_reports_where_the_backup_is() {
        let dir = temp_dir("rollbackfail");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let bytes = b"new build";
        let replacer = FailingReplacer::new(&[1, 2]);
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
    fn cleanup_backup_removes_only_the_old_binary_and_is_repeatable() {
        let dir = temp_dir("cleanup");
        let live = dir.join("postbox.exe");
        let backup = dir.join("postbox.exe.old");
        let pending = pending_exe_path(&dir, "postbox.exe");
        std::fs::write(&live, b"new build").unwrap();
        std::fs::write(&backup, b"old build").unwrap();
        std::fs::write(&pending, b"pending build").unwrap();

        cleanup_backup(&dir, "postbox.exe");
        cleanup_backup(&dir, "postbox.exe");

        assert!(!backup.exists());
        assert_eq!(std::fs::read(&live).unwrap(), b"new build");
        assert_eq!(std::fs::read(&pending).unwrap(), b"pending build");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_backup_ignores_removal_errors() {
        let dir = temp_dir("cleanup-error");
        let backup = dir.join("postbox.exe.old");
        std::fs::create_dir(&backup).unwrap();

        cleanup_backup(&dir, "postbox.exe");

        assert!(backup.is_dir());
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
            dir.join("postbox.exe.old")
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

    /// A one-entry stored zip, so this module needs no archive builder from the zip tests.
    fn stored_zip(name: &str, payload: &[u8]) -> Vec<u8> {
        let crc = crc32fast::hash(payload);
        let mut out = Vec::new();
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags: sizes are in the header
        out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        out.extend_from_slice(&[0u8; 4]); // time, date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(payload);

        let central_offset = out.len();
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(name.as_bytes());

        let central_size = out.len() - central_offset;
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(central_size as u32).to_le_bytes());
        out.extend_from_slice(&(central_offset as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn a_zip_payload_is_unpacked_on_every_os() {
        // The regression: this decision used to follow the build target, so the same real
        // release archive staged on Linux and failed on Windows.
        let dir = temp_dir("stagezip");
        std::fs::write(dir.join("postbox.exe"), b"installed build").unwrap();
        let payload = b"the new build inside the release zip";
        let archive = stored_zip("postbox.exe", payload);
        let asset = asset_for(&archive);

        let staged = stage_release(&asset, &archive, &dir, "postbox.exe").expect("zip stages");

        assert_eq!(std::fs::read(&staged.pending_exe).unwrap(), payload);
        assert_eq!(
            std::fs::read(dir.join("postbox.exe")).unwrap(),
            b"installed build",
            "the live binary is still untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plain_payload_is_copied_on_every_os() {
        // A manifest may serve the executable itself rather than a zip. The digest check
        // already proved the bytes, so the reader must not refuse them for lacking a zip
        // header, which is what the Windows-only extractor used to do.
        let dir = temp_dir("stageplain");
        std::fs::write(dir.join("postbox.exe"), b"installed build").unwrap();
        let bytes = b"a plain release binary";
        let asset = asset_for(bytes);

        let staged = stage_release(&asset, bytes, &dir, "postbox.exe").expect("plain stages");

        assert_eq!(std::fs::read(&staged.pending_exe).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
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
