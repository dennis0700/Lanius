//! Implements the `lanius update` subcommand: checks GitHub Releases for a
//! newer `lanius-cli` build, downloads and `minisign`-verifies the matching
//! platform archive (see [`lanius_core::update`]), and atomically swaps the
//! running binary in place.
//!
//! Deliberately narrow in scope compared to the desktop app's updater:
//! `lanius-cli` ships as a single static binary (see
//! `.github/workflows/linux-build.yml`), so there is no app bundle to
//! reassemble — just one file to replace. This module owns everything
//! specific to that: picking the right release asset for the running
//! platform, checking the binary's directory is actually writable before
//! downloading anything, refusing to run inside a container (where
//! replacing the binary in place makes no sense — the image should be
//! rebuilt/pulled instead), and the interactive confirmation prompt.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use lanius_core::update::{UpdateError, Updater, extract_tar_gz, is_newer};

/// Platform asset suffix used in release file names (see the `asset_suffix`
/// values in `.github/workflows/linux-build.yml`), or `None` if this build
/// isn't one `lanius-cli` publishes prebuilt archives for.
fn asset_suffix() -> Option<&'static str> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    if cfg!(target_arch = "x86_64") {
        Some("linux-amd64")
    } else if cfg!(target_arch = "aarch64") {
        Some("linux-arm64")
    } else {
        None
    }
}

/// Best-effort detection of running inside a container, where "replace the
/// binary on disk" is the wrong mental model — the image should be rebuilt
/// or re-pulled instead, or the next container restart just reverts the
/// update anyway.
fn running_in_container() -> bool {
    Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()
}

/// Runs the `update` subcommand: reports what it did and what to do next
/// via stdout, and returns an error (surfaced by `anyhow`/`main`) for
/// anything that stops the update from completing.
pub async fn run(check_only: bool, assume_yes: bool, pinned_version: Option<String>) -> Result<()> {
    if running_in_container() {
        bail!(
            "self-update is disabled inside a container; rebuild or pull a new image instead \
             (the container's filesystem is typically discarded on restart, so an in-place \
             update would not persist anyway)"
        );
    }

    let Some(suffix) = asset_suffix() else {
        bail!(
            "self-update isn't available for this platform/build; download a release manually \
             from https://github.com/{}/releases",
            lanius_core::update::REPO
        );
    };

    let updater = Updater::new(
        format!("lanius-cli/{}", lanius_core::config::APP_VERSION),
        std::env::var("VPN_PROXY_URL").ok().as_deref(),
    )
    .context("failed to build updater")?;

    let release = match &pinned_version {
        Some(version) => updater
            .release_by_version(version)
            .await
            .with_context(|| format!("failed to look up release {version:?}"))?,
        None => updater
            .latest_release()
            .await
            .context("failed to look up the latest release")?,
    };

    let current = semver::Version::parse(lanius_core::config::APP_VERSION)
        .context("running binary's own version is not valid semver (this is a bug)")?;
    let target = release
        .version()
        .with_context(|| format!("release {:?} has an invalid tag", release.tag_name))?;

    println!("current version : {current}");
    println!(
        "release found   : {} ({})",
        release.tag_name, release.html_url
    );

    let pinned = pinned_version.is_some();
    if !pinned && !is_newer(&current, &target) {
        println!("\nalready up to date.");
        return Ok(());
    }

    if check_only {
        println!("\na newer version is available: {target}");
        println!("run `lanius update` to install it.");
        return Ok(());
    }

    let asset_name = format!("lanius-{}-{suffix}.tar.gz", target);
    let asset = release
        .asset(&asset_name)
        .cloned()
        .ok_or_else(|| UpdateError::NoMatchingAsset {
            release: release.tag_name.clone(),
            expected: asset_name.clone(),
        })
        .context("no matching release asset")?;

    if !assume_yes && std::io::stdin().is_terminal() {
        print!("\nUpdate to {target}? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("aborted.");
            return Ok(());
        }
    }

    let exe =
        std::env::current_exe().context("failed to determine the running executable's path")?;
    let exe_dir = exe
        .parent()
        .context("running executable has no parent directory")?
        .to_path_buf();

    check_writable(&exe_dir)?;

    let work_dir = exe_dir.join(format!(".lanius-update-{}", std::process::id()));
    let cleanup = CleanupGuard(work_dir.clone());
    std::fs::create_dir_all(&work_dir)
        .context("failed to create a working directory next to the executable")?;

    println!("\ndownloading {asset_name} ({} bytes)...", asset.size);
    let archive_path = work_dir.join(&asset_name);
    let mut progress = StdoutProgress::default();
    updater
        .download_verified(&release, &asset, &archive_path, &mut progress)
        .await
        .context("download/verification failed")?;
    progress.finish();

    println!("extracting...");
    let extract_dir = work_dir.join("extracted");
    extract_tar_gz(&archive_path, &extract_dir)
        .await
        .context("failed to extract the downloaded archive")?;

    let new_binary = find_binary(&extract_dir, "lanius")
        .context("extracted archive did not contain a `lanius` binary")?;

    sanity_check(&new_binary).context("the downloaded binary failed a basic sanity check")?;

    println!("installing...");
    replace_binary(&exe, &new_binary).context("failed to replace the running binary")?;

    drop(cleanup);

    println!("\nupdated to {target}.");
    println!("if lanius is running as a systemd service, restart it to use the new binary:");
    println!("  sudo systemctl restart lanius");
    println!(
        "a backup of the previous binary was kept at {}.old",
        exe.display()
    );

    Ok(())
}

/// Confirms `dir` is writable *before* anything is downloaded, so a
/// permission problem surfaces immediately with an actionable message
/// rather than after a multi-second download.
fn check_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".lanius-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            bail!(
                "{} is not writable by the current user; re-run with sudo (e.g. `sudo lanius update`)",
                dir.display()
            )
        }
        Err(e) => Err(e).context(format!(
            "failed to check that {} is writable",
            dir.display()
        )),
    }
}

/// Searches `root` (breadth-first, capped at a shallow depth since release
/// archives are only a couple of levels deep) for a regular file literally
/// named `name`, returning its path.
fn find_binary(root: &Path, name: &str) -> Result<PathBuf> {
    let mut queue = vec![(root.to_path_buf(), 0u32)];
    while let Some((dir, depth)) = queue.pop() {
        if depth > 4 {
            continue;
        }
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) == Some(name) && path.is_file() {
                return Ok(path);
            }
            if path.is_dir() {
                queue.push((path, depth + 1));
            }
        }
    }
    bail!("no file named {name:?} found under {}", root.display())
}

/// Runs `binary --version` as a cheap smoke test that the downloaded file
/// is actually an executable for this platform (catches, e.g., an
/// architecture mismatch or a corrupted/incomplete extraction) before it
/// gets installed over the real binary.
fn sanity_check(binary: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(binary)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(binary, perms)?;
    }
    let status = std::process::Command::new(binary)
        .arg("--version")
        .status()
        .with_context(|| format!("failed to execute {}", binary.display()))?;
    if !status.success() {
        bail!("{} --version exited with {status}", binary.display());
    }
    Ok(())
}

/// Atomically replaces `exe` with `new_binary`'s contents: copies `exe` to
/// `{exe}.old` as a rollback point, writes the new content to a temp file
/// in the same directory as `exe` (so the final `rename` is on the same
/// filesystem and therefore atomic), then renames it into place.
///
/// A plain "copy over the running binary" would be observable as a
/// truncated/partial file to anything that opens it mid-write (e.g. a
/// concurrently starting instance); renaming a fully-written temp file over
/// it avoids that.
fn replace_binary(exe: &Path, new_binary: &Path) -> Result<()> {
    let old_backup = exe.with_extension("old");
    std::fs::copy(exe, &old_backup).context("failed to back up the current binary")?;

    let permissions = std::fs::metadata(exe)?.permissions();
    let tmp_path = exe.with_extension("update-tmp");
    std::fs::copy(new_binary, &tmp_path).context("failed to stage the new binary")?;
    std::fs::set_permissions(&tmp_path, permissions)
        .context("failed to set permissions on the new binary")?;

    std::fs::rename(&tmp_path, exe).context("failed to install the new binary")?;
    Ok(())
}

/// Removes the per-run working directory on drop, so a failed update
/// doesn't leave a `.lanius-update-<pid>` directory behind next to the
/// binary.
struct CleanupGuard(PathBuf);
impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A simple `stdout` progress reporter for [`Updater::download_verified`]:
/// prints an updating percentage line (or a byte counter if the asset's
/// total size is unknown), then a trailing newline once
/// [`finish`](Self::finish) is called.
#[derive(Default)]
struct StdoutProgress {
    last_percent: Option<u64>,
}

impl lanius_core::update::ProgressSink for StdoutProgress {
    fn on_progress(&mut self, downloaded: u64, total: u64) {
        use std::io::Write;
        if total > 0 {
            let percent = (downloaded * 100) / total.max(1);
            if self.last_percent != Some(percent) {
                self.last_percent = Some(percent);
                print!("\r  {percent:3}% ({downloaded}/{total} bytes)");
                std::io::stdout().flush().ok();
            }
        } else {
            print!("\r  {downloaded} bytes");
            std::io::stdout().flush().ok();
        }
    }
}

impl StdoutProgress {
    fn finish(&self) {
        println!();
    }
}
