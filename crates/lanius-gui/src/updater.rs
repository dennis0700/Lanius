//! Self-update support for the desktop app.
//!
//! Wraps [`lanius_core::update`] (release lookup + signed download, shared
//! with `lanius-cli`) with the one thing that's specific to the desktop
//! app: replacing a whole macOS `.app` bundle rather than a single binary,
//! then relaunching into it. `controller.rs` owns the only [`Updater`]
//! call sites: [`check`] from both the periodic background task and the
//! manual "Check for updates" button, and [`install`] (plus [`relaunch`])
//! from the "Update & restart" action.
//!
//! In-place install is only implemented for macOS (see
//! `.github/workflows/macos-build.yml`); on every other platform [`install`]
//! returns [`UpdaterError::UnsupportedPlatform`], and the UI falls back to
//! opening the release's GitHub page (see [`crate::controller::Controller::open_release_page`]
//! in `controller.rs`) instead of trying to install in place.
//!
//! Windows (`.github/workflows/windows-build.yml`) takes that fallback: its
//! [`asset_name`] lets [`check`] notice a new release as long as the
//! release carries a Windows asset, and "Update & restart" then opens the
//! release page for a manual download.

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use lanius_core::update::extract_tar_gz;
use lanius_core::update::{Asset, Release, UpdateError, Updater, is_newer};

/// Everything [`install`] needs that [`check`] already had to fetch, so a
/// single "check" -> "install" flow only ever calls the GitHub API once.
#[derive(Debug, Clone)]
pub struct AvailableUpdate {
    pub release: Release,
    // Only read by the macOS in-place installer; elsewhere its presence in a
    // release is just the signal that this platform has a build to update to.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub asset: Asset,
    pub version: semver::Version,
}

/// Errors specific to this module; network/signature failures from
/// [`lanius_core::update`] are wrapped via `#[from]` rather than
/// re-described, since their messages are already user-presentable.
#[derive(Debug, thiserror::Error)]
pub enum UpdaterError {
    #[error(transparent)]
    Update(#[from] UpdateError),

    // Only constructed on non-macOS builds (see the `install`/`relaunch`
    // stubs below), since macOS is the only platform with a real update
    // path today — kept for every platform so `UpdaterError` itself is
    // platform-independent.
    #[allow(dead_code)]
    #[error("this platform/build has no downloadable update asset")]
    UnsupportedPlatform,

    // The variants below are only constructed by the macOS installer; the
    // same "keep the enum platform-independent" reasoning applies.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    #[error(
        "Lanius is running from a temporary, read-only copy (macOS Gatekeeper's app translocation); \
         move Lanius.app to /Applications and reopen it before updating"
    )]
    Translocated,

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    #[error("{0} is not writable; move Lanius.app somewhere you can write to (e.g. /Applications)")]
    NotWritable(PathBuf),

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    #[error("could not determine the running app bundle's location: {0}")]
    NotABundle(String),

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    #[error("the downloaded update did not contain a valid Lanius.app bundle")]
    InvalidBundle,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Name of this build's published update asset for `version`, matching the
/// naming produced by the release workflows, or `None` if this build has
/// no published asset:
/// - macOS: `.github/workflows/macos-build.yml`'s
///   `Lanius-{version}-arm64.app.tar.gz` (downloaded and installed in place);
/// - Windows: `.github/workflows/windows-build.yml`'s
///   `Lanius-{version}-windows-x64.zip` (used only to detect that a Windows
///   build exists for that release; see the module docs).
fn asset_name(version: &semver::Version) -> Option<String> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(format!("Lanius-{version}-arm64.app.tar.gz"))
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(format!("Lanius-{version}-windows-x64.zip"))
    } else {
        None
    }
}

fn build_updater() -> Result<Updater, UpdaterError> {
    Updater::new(
        format!("lanius-desktop/{}", lanius_core::config::APP_VERSION),
        None,
    )
    .map_err(UpdaterError::Update)
}

/// Checks GitHub Releases for a version newer than the running app.
/// Returns `Ok(None)` when already up to date (including when the running
/// platform simply isn't one this build path supports — there's nothing
/// actionable to report either way).
///
/// # Examples
///
/// ```ignore
/// if let Some(update) = crate::updater::check().await? {
///     tracing::info!("update available: {}", update.version);
/// }
/// ```
pub async fn check() -> Result<Option<AvailableUpdate>, UpdaterError> {
    let updater = build_updater()?;
    let release = updater.latest_release().await?;
    let current = semver::Version::parse(lanius_core::config::APP_VERSION).map_err(|e| {
        UpdateError::InvalidVersion(lanius_core::config::APP_VERSION.to_string(), e)
    })?;
    let target = release.version()?;

    if !is_newer(&current, &target) {
        return Ok(None);
    }

    let Some(asset_name) = asset_name(&target) else {
        return Ok(None);
    };
    let Some(asset) = release.asset(&asset_name).cloned() else {
        return Ok(None);
    };

    Ok(Some(AvailableUpdate {
        release,
        asset,
        version: target,
    }))
}

/// Downloads, verifies, and installs `update` in place of the currently
/// running app bundle, calling `on_progress(downloaded, total)` as bytes
/// arrive. Returns the path to the newly installed bundle on success —
/// callers must still call [`relaunch`] (after shutting down the embedded
/// gateway) to actually switch the running process over to it, since this
/// function does not touch the current process.
///
/// Not implemented on non-macOS platforms — see the module docs.
///
/// # Examples
///
/// ```ignore
/// if let Some(update) = crate::updater::check().await? {
///     let bundle = crate::updater::install(&update, |done, total| {
///         tracing::debug!("downloaded {done}/{total} bytes");
///     }).await?;
/// }
/// ```
#[cfg(target_os = "macos")]
pub async fn install(
    update: &AvailableUpdate,
    mut on_progress: impl FnMut(u64, u64) + Send,
) -> Result<PathBuf, UpdaterError> {
    let current_bundle = current_app_bundle()?;
    ensure_installable(&current_bundle)?;

    let parent = current_bundle
        .parent()
        .ok_or_else(|| UpdaterError::NotABundle("app bundle has no parent directory".into()))?;
    let work_dir = parent.join(format!(".lanius-update-{}", std::process::id()));
    tokio::fs::create_dir_all(&work_dir).await?;
    let cleanup = CleanupGuard(work_dir.clone());

    let updater = build_updater()?;
    let archive_path = work_dir.join(&update.asset.name);
    updater
        .download_verified(
            &update.release,
            &update.asset,
            &archive_path,
            &mut on_progress,
        )
        .await?;

    let extract_dir = work_dir.join("extracted");
    extract_tar_gz(&archive_path, &extract_dir).await?;

    let new_bundle = find_app_bundle(&extract_dir).ok_or(UpdaterError::InvalidBundle)?;
    verify_bundle(&new_bundle)?;

    let installed = swap_bundle(&current_bundle, &new_bundle)?;
    drop(cleanup);
    Ok(installed)
}

/// Non-macOS stub: always fails with [`UpdaterError::UnsupportedPlatform`].
///
/// # Examples
///
/// ```ignore
/// let result = crate::updater::install(&update, |_, _| {}).await;
/// assert!(matches!(result, Err(crate::updater::UpdaterError::UnsupportedPlatform)));
/// ```
#[cfg(not(target_os = "macos"))]
pub async fn install(
    _update: &AvailableUpdate,
    _on_progress: impl FnMut(u64, u64) + Send,
) -> Result<PathBuf, UpdaterError> {
    Err(UpdaterError::UnsupportedPlatform)
}

/// Relaunches into the app bundle at `path`, replacing the current process
/// as far as the user can tell. Callers must have already stopped the
/// embedded gateway (and any other cleanup `Controller::shutdown` does)
/// before calling this, since the current process exits shortly after this
/// returns (via the caller quitting the Slint event loop) while the new
/// process is still starting up.
///
/// Implemented by spawning a detached shell command that waits for this
/// process's PID to disappear before running `open -n <path>`: opening
/// immediately (before this process has actually exited and released its
/// port/lock) would risk the new instance colliding with the old one, and
/// `open` *without* `-n` would just refocus the still-running old instance
/// instead of launching the new bundle.
///
/// # Examples
///
/// ```ignore
/// let bundle = crate::updater::install(&update, |_, _| {}).await?;
/// // ...stop the embedded gateway first, then:
/// crate::updater::relaunch(&bundle)?;
/// slint::quit_event_loop()?;
/// ```
#[cfg(target_os = "macos")]
pub fn relaunch(path: &Path) -> Result<(), UpdaterError> {
    let pid = std::process::id();
    let script = format!(
        "while kill -0 {pid} 2>/dev/null; do sleep 0.2; done; open -n {}",
        shell_quote(path)
    );
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .spawn()?;
    Ok(())
}

/// Non-macOS stub: always fails with [`UpdaterError::UnsupportedPlatform`].
///
/// # Examples
///
/// ```ignore
/// let result = crate::updater::relaunch(std::path::Path::new("/Applications/Lanius.app"));
/// assert!(matches!(result, Err(crate::updater::UpdaterError::UnsupportedPlatform)));
/// ```
#[cfg(not(target_os = "macos"))]
pub fn relaunch(_path: &Path) -> Result<(), UpdaterError> {
    Err(UpdaterError::UnsupportedPlatform)
}

/// Opens `url` in the user's default browser, for the fallback path on
/// platforms/situations where in-app install isn't available.
///
/// # Examples
///
/// ```ignore
/// if let Some(update) = crate::updater::check().await? {
///     crate::updater::open_url(&update.release.html_url);
/// }
/// ```
pub fn open_url(url: &str) {
    #[cfg(target_os = "windows")]
    if let Err(e) = crate::windows::open_url(url) {
        tracing::warn!("failed to open {url} in a browser: {e}");
    }

    #[cfg(not(target_os = "windows"))]
    {
        #[cfg(target_os = "macos")]
        let cmd = "open";
        #[cfg(not(target_os = "macos"))]
        let cmd = "xdg-open";

        if let Err(e) = std::process::Command::new(cmd).arg(url).spawn() {
            tracing::warn!("failed to open {url} in a browser: {e}");
        }
    }
}

/// Quotes `path` for safe interpolation into a `/bin/sh -c` script (single
/// quotes, with any embedded `'` escaped by ending the quoted string,
/// emitting an escaped literal quote, then resuming it).
#[cfg(target_os = "macos")]
fn shell_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\'', "'\\''"))
}

/// Removes the per-run working directory on drop, so a failed/interrupted
/// update doesn't leave a `.lanius-update-<pid>` directory next to the app
/// bundle.
#[cfg(target_os = "macos")]
struct CleanupGuard(PathBuf);
#[cfg(target_os = "macos")]
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Resolves the running executable's path back to its enclosing `.app`
/// bundle (`.../Lanius.app/Contents/MacOS/lanius-desktop` ->
/// `.../Lanius.app`), failing if the running binary isn't actually inside
/// a bundle shaped like that — e.g. a plain `cargo run` build.
#[cfg(target_os = "macos")]
fn current_app_bundle() -> Result<PathBuf, UpdaterError> {
    let exe = std::env::current_exe()
        .map_err(|e| UpdaterError::NotABundle(format!("current_exe failed: {e}")))?;
    let bundle = exe
        .parent() // Contents/MacOS
        .and_then(Path::parent) // Contents
        .and_then(Path::parent) // Lanius.app
        .ok_or_else(|| UpdaterError::NotABundle(exe.display().to_string()))?;
    if bundle.extension().and_then(|e| e.to_str()) != Some("app") {
        return Err(UpdaterError::NotABundle(exe.display().to_string()));
    }
    Ok(bundle.to_path_buf())
}

/// Refuses to proceed if `bundle` is running from macOS's app translocation
/// path (a read-only, randomized copy Gatekeeper uses for apps that
/// haven't been moved out of e.g. `~/Downloads` yet — writing back to it
/// would be pointless since translocation discards it), or if its parent
/// directory isn't writable by the current user.
#[cfg(target_os = "macos")]
fn ensure_installable(bundle: &Path) -> Result<(), UpdaterError> {
    let path_str = bundle.to_string_lossy();
    if path_str.contains("/AppTranslocation/") {
        return Err(UpdaterError::Translocated);
    }

    let parent = bundle
        .parent()
        .ok_or_else(|| UpdaterError::NotABundle("app bundle has no parent directory".into()))?;
    let probe = parent.join(format!(".lanius-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(UpdaterError::NotWritable(parent.to_path_buf()))
        }
        Err(e) => Err(UpdaterError::Io(e)),
    }
}

/// Finds the single top-level `*.app` bundle directly under `root` (an
/// extracted release archive), the same shape produced by
/// `.github/workflows/macos-build.yml`'s `dist/Lanius.app`.
#[cfg(target_os = "macos")]
fn find_app_bundle(root: &Path) -> Option<PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok())
        .find_map(|entry| {
            let path = entry.path();
            (path.is_dir() && path.extension().and_then(|e| e.to_str()) == Some("app"))
                .then_some(path)
        })
}

/// Sanity-checks that `bundle` actually contains a `lanius-desktop`
/// executable (catching a corrupted/incomplete extraction) and marks it
/// executable, since `tar` extraction does not always preserve the
/// original executable bit depending on how the archive was produced.
#[cfg(target_os = "macos")]
fn verify_bundle(bundle: &Path) -> Result<(), UpdaterError> {
    use std::os::unix::fs::PermissionsExt;
    let binary = bundle.join("Contents/MacOS/lanius-desktop");
    if !binary.is_file() {
        return Err(UpdaterError::InvalidBundle);
    }
    let mut perms = std::fs::metadata(&binary)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&binary, perms)?;
    Ok(())
}

/// Atomically swaps `current_bundle` out for `new_bundle`: renames the
/// current bundle aside as a timestamped backup, moves `new_bundle` into
/// `current_bundle`'s place, and — if that second rename fails — restores
/// the backup so a failed update never leaves the app uninstalled. The
/// backup itself is left on disk (not cleaned up) as a manual rollback
/// point; it's small compared to a full reinstall and macOS won't reuse
/// the name on its own.
#[cfg(target_os = "macos")]
fn swap_bundle(current_bundle: &Path, new_bundle: &Path) -> Result<PathBuf, UpdaterError> {
    let backup = current_bundle.with_extension(format!(
        "app.old-{}",
        chrono::Local::now().format("%Y%m%d%H%M%S")
    ));
    std::fs::rename(current_bundle, &backup)?;

    match std::fs::rename(new_bundle, current_bundle) {
        Ok(()) => Ok(current_bundle.to_path_buf()),
        Err(e) => {
            // Best-effort rollback: put the original bundle back so the
            // app is never left uninstalled by a failed update.
            let _ = std::fs::rename(&backup, current_bundle);
            Err(UpdaterError::Io(e))
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        let path = Path::new("/Applications/Lan'ius.app");
        assert_eq!(shell_quote(path), "'/Applications/Lan'\\''ius.app'");
    }

    #[test]
    fn find_app_bundle_locates_the_only_dot_app_directory() {
        let dir = std::env::temp_dir().join(format!("lanius-updater-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Lanius.app/Contents/MacOS")).unwrap();
        std::fs::write(
            dir.join("Lanius.app/Contents/MacOS/lanius-desktop"),
            b"stub",
        )
        .unwrap();

        let found = find_app_bundle(&dir).expect("must find the bundle");
        assert_eq!(found, dir.join("Lanius.app"));

        verify_bundle(&found).expect("verify must pass for a bundle with the right binary");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_installable_rejects_translocated_paths() {
        let path = Path::new("/private/var/folders/00/xyz/AppTranslocation/ABCDEF/d/Lanius.app");
        assert!(matches!(
            ensure_installable(path),
            Err(UpdaterError::Translocated)
        ));
    }
}

/// Pins each platform's update asset name to what its release workflow
/// uploads — a mismatch silently stops that platform from ever seeing an
/// update, which no other test would catch.
#[cfg(test)]
mod asset_name_tests {
    use super::asset_name;

    #[test]
    fn asset_name_matches_release_workflow_naming() {
        let version = semver::Version::new(1, 2, 3);
        let expected = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some("Lanius-1.2.3-arm64.app.tar.gz")
        } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some("Lanius-1.2.3-windows-x64.zip")
        } else {
            None
        };
        assert_eq!(asset_name(&version).as_deref(), expected);
    }
}
