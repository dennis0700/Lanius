//! Self-update support shared by `lanius-cli` and `lanius-gui`.
//!
//! Both binaries ship as GitHub Release assets (see the repo's
//! `.github/workflows/*.yml`), and both need the same three primitives: look
//! up the latest (or a specific) release, pick the asset that matches the
//! running platform, and download it with a verified `minisign` signature
//! before anything touches disk permanently. This module implements those
//! primitives once; `lanius-cli`'s `update` subcommand and `lanius-gui`'s
//! updater own the platform-specific install step (replacing a single
//! binary vs. swapping a macOS `.app` bundle) on top of it.
//!
//! Every release asset is expected to be published alongside a detached
//! `<asset>.minisig` signature (see the CI workflows), signed with the
//! private half of [`RELEASE_PUBLIC_KEY`]. [`Updater::download_verified`]
//! refuses to return any bytes that don't verify against that key *and*
//! whose signature's trusted comment doesn't name the exact asset that was
//! requested — the latter stops a valid signature for one file from being
//! replayed against a different one.

use std::path::Path;
use std::time::Duration;

use minisign_verify::{PublicKey, Signature};
use serde::Deserialize;

/// GitHub `owner/repo` slug releases are published under.
pub const REPO: &str = "dennis0700/Lanius";

/// `minisign` public key used to verify every release asset's signature.
/// The matching private key is held outside the repo (see
/// `deploy/lanius-release.pub` and the release workflows' `MINISIGN_SECRET_KEY`
/// secret) and never checked in.
pub const RELEASE_PUBLIC_KEY: &str = "RWQDB3DXVK72nCKQJoEzOip8+lQuG1yfL90+TKkNbhoOSYryxJeNWvf3";

const GITHUB_API_BASE: &str = "https://api.github.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Everything that can go wrong while checking for or fetching an update.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("GitHub API returned {status}: {body}")]
    Api { status: u16, body: String },

    #[error("no release matching {0:?} was found")]
    ReleaseNotFound(String),

    #[error("release {release} has no asset for this platform (looked for {expected})")]
    NoMatchingAsset { release: String, expected: String },

    #[error("release {release} is missing a .minisig signature for {asset}")]
    MissingSignature { release: String, asset: String },

    #[error("signature verification failed for {0}: {1}")]
    SignatureInvalid(String, minisign_verify::Error),

    #[error(
        "signature for {asset} does not name this file (trusted comment {comment:?}); refusing to trust it"
    )]
    SignatureMismatch { asset: String, comment: String },

    #[error("failed to parse version {0:?}: {1}")]
    InvalidVersion(String, semver::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// A single GitHub Release, trimmed to the fields this module needs.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    /// The release's tag, e.g. `"v0.2.0"`.
    pub tag_name: String,
    /// The release's own HTML page, shown to users who can't be auto-updated
    /// (unsupported platform, permission failure, etc.).
    pub html_url: String,
    pub prerelease: bool,
    pub draft: bool,
    pub assets: Vec<Asset>,
}

impl Release {
    /// Parses [`tag_name`](Self::tag_name) as a [`semver::Version`],
    /// stripping a leading `v` if present (GitHub tags are conventionally
    /// `v1.2.3`, but `Cargo.toml`/`semver` expect `1.2.3`).
    pub fn version(&self) -> Result<semver::Version, UpdateError> {
        let raw = self.tag_name.trim_start_matches('v');
        semver::Version::parse(raw)
            .map_err(|e| UpdateError::InvalidVersion(self.tag_name.clone(), e))
    }

    /// Looks up an asset by exact file name.
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// A single downloadable file attached to a [`Release`].
#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub browser_download_url: String,
}

/// Reports progress while [`Updater::download_verified`] streams an asset,
/// so callers (the CLI's progress line, the GUI's progress bar) can show
/// something better than a frozen UI during a multi-second download.
pub trait ProgressSink: Send {
    /// Called after each chunk is written, with the running total and the
    /// asset's advertised total size (`0` if unknown).
    fn on_progress(&mut self, downloaded: u64, total: u64);
}

/// A no-op [`ProgressSink`] for callers that don't care about progress.
pub struct NoProgress;
impl ProgressSink for NoProgress {
    fn on_progress(&mut self, _downloaded: u64, _total: u64) {}
}

impl<F: FnMut(u64, u64) + Send> ProgressSink for F {
    fn on_progress(&mut self, downloaded: u64, total: u64) {
        self(downloaded, total)
    }
}

/// Looks up releases and downloads verified assets from a GitHub
/// repository. Constructed once per update check; cheap to build (it just
/// wraps a `reqwest::Client`).
pub struct Updater {
    client: reqwest::Client,
    api_base: String,
    repo: String,
    public_key: PublicKey,
}

impl Updater {
    /// Builds an [`Updater`] against the real `lanius` GitHub repository and
    /// the embedded [`RELEASE_PUBLIC_KEY`], with `user_agent` identifying
    /// the calling binary (GitHub requires a `User-Agent` on API requests).
    ///
    /// `proxy_url`, if set, is used for both the GitHub API and asset
    /// download requests, mirroring `VPN_PROXY_URL`'s scheme validation
    /// elsewhere in this crate (`http(s)://`, `socks5://`, `socks5h://`).
    pub fn new(
        user_agent: impl Into<String>,
        proxy_url: Option<&str>,
    ) -> Result<Self, UpdateError> {
        let public_key = PublicKey::from_base64(RELEASE_PUBLIC_KEY)
            .expect("RELEASE_PUBLIC_KEY must be a valid minisign public key");
        Self::with_config(
            GITHUB_API_BASE.to_string(),
            REPO.to_string(),
            public_key,
            user_agent,
            proxy_url,
        )
    }

    /// Like [`new`](Self::new), but with the API base URL, repo slug, and
    /// public key overridable — used by this module's own tests to point at
    /// a local mock server and a throwaway keypair instead of the real
    /// GitHub API and production signing key.
    pub fn with_config(
        api_base: String,
        repo: String,
        public_key: PublicKey,
        user_agent: impl Into<String>,
        proxy_url: Option<&str>,
    ) -> Result<Self, UpdateError> {
        let mut builder = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(user_agent.into());
        if let Some(proxy_url) = proxy_url.filter(|u| !u.is_empty()) {
            builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
        }
        Ok(Self {
            client: builder.build()?,
            api_base,
            repo,
            public_key,
        })
    }

    /// Fetches the latest non-prerelease, non-draft release.
    pub async fn latest_release(&self) -> Result<Release, UpdateError> {
        let url = format!("{}/repos/{}/releases/latest", self.api_base, self.repo);
        self.get_release(&url).await
    }

    /// Fetches the release tagged `version` (accepts either `"1.2.3"` or
    /// `"v1.2.3"`), used for `--version <x.y.z>` pins/rollbacks.
    pub async fn release_by_version(&self, version: &str) -> Result<Release, UpdateError> {
        let tag = if version.starts_with('v') {
            version.to_string()
        } else {
            format!("v{version}")
        };
        let url = format!("{}/repos/{}/releases/tags/{tag}", self.api_base, self.repo);
        self.get_release(&url).await
    }

    async fn get_release(&self, url: &str) -> Result<Release, UpdateError> {
        let response = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(UpdateError::ReleaseNotFound(url.to_string()));
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(UpdateError::Api { status, body });
        }
        Ok(response.json().await?)
    }

    /// Downloads `asset` from `release`, streaming it to `dest` while
    /// verifying it against the same release's `<asset>.minisig` signature.
    ///
    /// The signature's trusted comment must be exactly `file:{asset.name}`
    /// (see the signing step in `.github/workflows/*.yml`); anything else is
    /// rejected via [`UpdateError::SignatureMismatch`] even if the
    /// underlying Ed25519 signature is otherwise valid, so a correctly
    /// signed file can never be reused to smuggle in different bytes under
    /// a different name.
    ///
    /// Writes to a `dest.part` sibling file first and only renames it into
    /// place once the signature is confirmed, so a failed/interrupted
    /// download or a failed verification never leaves a file at `dest` for
    /// a caller to mistakenly trust.
    pub async fn download_verified(
        &self,
        release: &Release,
        asset: &Asset,
        dest: &Path,
        progress: &mut dyn ProgressSink,
    ) -> Result<(), UpdateError> {
        let sig_name = format!("{}.minisig", asset.name);
        let sig_asset = release
            .asset(&sig_name)
            .ok_or_else(|| UpdateError::MissingSignature {
                release: release.tag_name.clone(),
                asset: asset.name.clone(),
            })?;

        let sig_text = self
            .client
            .get(&sig_asset.browser_download_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let signature = Signature::decode(&sig_text)
            .map_err(|e| UpdateError::SignatureInvalid(asset.name.clone(), e))?;

        let expected_comment = format!("file:{}", asset.name);
        if signature.trusted_comment() != expected_comment {
            return Err(UpdateError::SignatureMismatch {
                asset: asset.name.clone(),
                comment: signature.trusted_comment().to_string(),
            });
        }

        let part_path = dest.with_extension("part");
        {
            let response = self
                .client
                .get(&asset.browser_download_url)
                .timeout(DOWNLOAD_TIMEOUT)
                .send()
                .await?
                .error_for_status()?;

            let mut file = tokio::fs::File::create(&part_path).await?;
            let mut downloaded = 0u64;
            let mut stream = response.bytes_stream();
            use futures_util::StreamExt;
            use tokio::io::AsyncWriteExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                file.write_all(&chunk).await?;
                downloaded += chunk.len() as u64;
                progress.on_progress(downloaded, asset.size);
            }
            file.flush().await?;
        }

        let bytes = tokio::fs::read(&part_path).await?;
        let verify_result = self.public_key.verify(&bytes, &signature, false);
        if let Err(e) = verify_result {
            let _ = tokio::fs::remove_file(&part_path).await;
            return Err(UpdateError::SignatureInvalid(asset.name.clone(), e));
        }

        tokio::fs::rename(&part_path, dest).await?;
        Ok(())
    }
}

/// Extracts a `.tar.gz` archive at `archive_path` into `dest_dir`, which is
/// created if it doesn't already exist.
///
/// Runs the actual (blocking, CPU-bound) decompression/unpacking on a
/// `spawn_blocking` thread so it doesn't stall the async runtime; `tar`'s
/// `Archive::unpack` already rejects entries that would escape `dest_dir`
/// via `..` path components.
pub async fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<(), UpdateError> {
    let archive_path = archive_path.to_path_buf();
    let dest_dir = dest_dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&dest_dir)?;
        let file = std::fs::File::open(&archive_path)?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(&dest_dir)
    })
    .await
    .map_err(|e| UpdateError::Io(std::io::Error::other(e)))??;
    Ok(())
}

/// Returns whether `latest` is a newer version than `current`, i.e.
/// whether an update should be offered. Uses plain semver ordering — a
/// pre-release `latest` (e.g. `1.2.0-beta.1`) is deliberately *not*
/// considered newer than a stable `current` of the same base version, since
/// [`Updater::latest_release`] already excludes prereleases and this guards
/// against a `--version` pin or a future "beta channel" flag from ever
/// downgrading a stable install without the user asking to.
pub fn is_newer(current: &semver::Version, latest: &semver::Version) -> bool {
    latest > current
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Path as AxPath;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::{Json, Router};
    use std::sync::Arc;

    struct TestServer {
        addr: std::net::SocketAddr,
    }

    async fn spawn_test_server(
        release_json: serde_json::Value,
        files: Vec<(&'static str, Vec<u8>)>,
    ) -> TestServer {
        let files: Arc<Vec<(&'static str, Vec<u8>)>> = Arc::new(files);
        let release_json = Arc::new(release_json);

        let app = Router::new()
            .route(
                "/repos/test/repo/releases/latest",
                get({
                    let release_json = Arc::clone(&release_json);
                    move || {
                        let release_json = Arc::clone(&release_json);
                        async move { Json((*release_json).clone()) }
                    }
                }),
            )
            .route(
                "/repos/test/repo/releases/tags/{tag}",
                get({
                    let release_json = Arc::clone(&release_json);
                    move |AxPath(_tag): AxPath<String>| {
                        let release_json = Arc::clone(&release_json);
                        async move { Json((*release_json).clone()) }
                    }
                }),
            )
            .route(
                "/assets/{name}",
                get(move |AxPath(name): AxPath<String>| {
                    let files = Arc::clone(&files);
                    async move {
                        match files.iter().find(|(n, _)| *n == name) {
                            Some((_, bytes)) => bytes.clone().into_response(),
                            None => axum::http::StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        TestServer { addr }
    }

    fn sign(
        sk: &minisign::SecretKey,
        pk: &minisign::PublicKey,
        data: &[u8],
        comment: &str,
    ) -> String {
        minisign::sign(Some(pk), sk, data, Some(comment), None)
            .unwrap()
            .into_string()
    }

    fn keypair() -> (minisign::KeyPair, PublicKey) {
        let kp = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let verify_key = PublicKey::from_base64(&kp.pk.to_base64()).unwrap();
        (kp, verify_key)
    }

    #[tokio::test]
    async fn latest_release_parses_version_and_assets() {
        let (kp, verify_key) = keypair();
        let asset_bytes = b"binary-contents-v1".to_vec();
        let sig = sign(
            &kp.sk,
            &kp.pk,
            &asset_bytes,
            "file:lanius-1.0.0-linux-amd64.tar.gz",
        );

        let server = spawn_test_server(
            serde_json::json!({
                "tag_name": "v1.0.0",
                "html_url": "https://example.com/releases/v1.0.0",
                "prerelease": false,
                "draft": false,
                "assets": [
                    {
                        "name": "lanius-1.0.0-linux-amd64.tar.gz",
                        "size": asset_bytes.len(),
                        "browser_download_url": "http://placeholder/assets/lanius-1.0.0-linux-amd64.tar.gz",
                    },
                    {
                        "name": "lanius-1.0.0-linux-amd64.tar.gz.minisig",
                        "size": sig.len(),
                        "browser_download_url": "http://placeholder/assets/lanius-1.0.0-linux-amd64.tar.gz.minisig",
                    },
                ],
            }),
            vec![
                ("lanius-1.0.0-linux-amd64.tar.gz", asset_bytes.clone()),
                ("lanius-1.0.0-linux-amd64.tar.gz.minisig", sig.into_bytes()),
            ],
        )
        .await;

        let updater = Updater::with_config(
            format!("http://{}", server.addr),
            "test/repo".to_string(),
            verify_key,
            "lanius-test/1.0",
            None,
        )
        .unwrap();

        let release = updater.latest_release().await.unwrap();
        assert_eq!(release.version().unwrap(), semver::Version::new(1, 0, 0));
        assert!(!release.prerelease);
        assert!(release.asset("lanius-1.0.0-linux-amd64.tar.gz").is_some());
    }

    #[tokio::test]
    async fn download_verified_accepts_a_correctly_signed_asset() {
        let (kp, verify_key) = keypair();
        let asset_bytes = b"totally real binary".to_vec();
        let sig = sign(&kp.sk, &kp.pk, &asset_bytes, "file:app.tar.gz");

        let server = spawn_test_server(
            serde_json::json!({}),
            vec![
                ("app.tar.gz", asset_bytes.clone()),
                ("app.tar.gz.minisig", sig.into_bytes()),
            ],
        )
        .await;

        let updater = Updater::with_config(
            format!("http://{}", server.addr),
            "test/repo".to_string(),
            verify_key,
            "lanius-test/1.0",
            None,
        )
        .unwrap();

        let release = Release {
            tag_name: "v1.0.0".into(),
            html_url: String::new(),
            prerelease: false,
            draft: false,
            assets: vec![
                Asset {
                    name: "app.tar.gz".into(),
                    size: asset_bytes.len() as u64,
                    browser_download_url: format!("http://{}/assets/app.tar.gz", server.addr),
                },
                Asset {
                    name: "app.tar.gz.minisig".into(),
                    size: 0,
                    browser_download_url: format!(
                        "http://{}/assets/app.tar.gz.minisig",
                        server.addr
                    ),
                },
            ],
        };
        let asset = release.asset("app.tar.gz").unwrap().clone();

        let dir = std::env::temp_dir().join(format!("lanius-update-test-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("app.tar.gz");

        updater
            .download_verified(&release, &asset, &dest, &mut NoProgress)
            .await
            .unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), asset_bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn download_verified_rejects_a_tampered_asset() {
        let (kp, verify_key) = keypair();
        let signed_bytes = b"original bytes".to_vec();
        let sig = sign(&kp.sk, &kp.pk, &signed_bytes, "file:app.tar.gz");
        // Serve different bytes than what was actually signed.
        let tampered_bytes = b"tampered!!bytes".to_vec();

        let server = spawn_test_server(
            serde_json::json!({}),
            vec![
                ("app.tar.gz", tampered_bytes),
                ("app.tar.gz.minisig", sig.into_bytes()),
            ],
        )
        .await;

        let updater = Updater::with_config(
            format!("http://{}", server.addr),
            "test/repo".to_string(),
            verify_key,
            "lanius-test/1.0",
            None,
        )
        .unwrap();

        let release = Release {
            tag_name: "v1.0.0".into(),
            html_url: String::new(),
            prerelease: false,
            draft: false,
            assets: vec![
                Asset {
                    name: "app.tar.gz".into(),
                    size: signed_bytes_len(&signed_bytes),
                    browser_download_url: format!("http://{}/assets/app.tar.gz", server.addr),
                },
                Asset {
                    name: "app.tar.gz.minisig".into(),
                    size: 0,
                    browser_download_url: format!(
                        "http://{}/assets/app.tar.gz.minisig",
                        server.addr
                    ),
                },
            ],
        };
        let asset = release.asset("app.tar.gz").unwrap().clone();

        let dir = std::env::temp_dir().join(format!("lanius-update-test-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("app.tar.gz");

        let result = updater
            .download_verified(&release, &asset, &dest, &mut NoProgress)
            .await;
        assert!(matches!(result, Err(UpdateError::SignatureInvalid(_, _))));
        assert!(!dest.exists(), "tampered download must not be left at dest");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn download_verified_rejects_a_signature_for_a_different_file() {
        let (kp, verify_key) = keypair();
        let asset_bytes = b"some bytes".to_vec();
        // Signed for a different asset name than the one we'll request.
        let sig = sign(&kp.sk, &kp.pk, &asset_bytes, "file:other-name.tar.gz");

        let server = spawn_test_server(
            serde_json::json!({}),
            vec![
                ("app.tar.gz", asset_bytes.clone()),
                ("app.tar.gz.minisig", sig.into_bytes()),
            ],
        )
        .await;

        let updater = Updater::with_config(
            format!("http://{}", server.addr),
            "test/repo".to_string(),
            verify_key,
            "lanius-test/1.0",
            None,
        )
        .unwrap();

        let release = Release {
            tag_name: "v1.0.0".into(),
            html_url: String::new(),
            prerelease: false,
            draft: false,
            assets: vec![
                Asset {
                    name: "app.tar.gz".into(),
                    size: asset_bytes.len() as u64,
                    browser_download_url: format!("http://{}/assets/app.tar.gz", server.addr),
                },
                Asset {
                    name: "app.tar.gz.minisig".into(),
                    size: 0,
                    browser_download_url: format!(
                        "http://{}/assets/app.tar.gz.minisig",
                        server.addr
                    ),
                },
            ],
        };
        let asset = release.asset("app.tar.gz").unwrap().clone();

        let dir = std::env::temp_dir().join(format!("lanius-update-test-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("app.tar.gz");

        let result = updater
            .download_verified(&release, &asset, &dest, &mut NoProgress)
            .await;
        assert!(matches!(result, Err(UpdateError::SignatureMismatch { .. })));
        assert!(!dest.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_signature_asset_is_reported_before_any_download() {
        let asset_bytes = b"no signature for this one".to_vec();
        let server = spawn_test_server(
            serde_json::json!({}),
            vec![("app.tar.gz", asset_bytes.clone())],
        )
        .await;

        let (_, verify_key) = keypair();
        let updater = Updater::with_config(
            format!("http://{}", server.addr),
            "test/repo".to_string(),
            verify_key,
            "lanius-test/1.0",
            None,
        )
        .unwrap();

        let release = Release {
            tag_name: "v1.0.0".into(),
            html_url: String::new(),
            prerelease: false,
            draft: false,
            assets: vec![Asset {
                name: "app.tar.gz".into(),
                size: asset_bytes.len() as u64,
                browser_download_url: format!("http://{}/assets/app.tar.gz", server.addr),
            }],
        };
        let asset = release.asset("app.tar.gz").unwrap().clone();

        let dir = std::env::temp_dir().join(format!("lanius-update-test-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("app.tar.gz");

        let result = updater
            .download_verified(&release, &asset, &dest, &mut NoProgress)
            .await;
        assert!(matches!(result, Err(UpdateError::MissingSignature { .. })));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The embedded [`RELEASE_PUBLIC_KEY`] and the committed
    /// `deploy/lanius-release.pub` (kept around for operators to verify
    /// release assets manually, e.g. with the `minisign`/`rsign2` CLI) must
    /// name the exact same key, or a release signed against one would
    /// silently fail to verify against the other.
    #[test]
    fn embedded_public_key_matches_the_committed_pub_file() {
        let pub_file = include_str!("../../../deploy/lanius-release.pub");
        let key_line = pub_file
            .lines()
            .find(|line| !line.starts_with("untrusted comment:"))
            .expect("pub file must have a key line");
        assert_eq!(key_line.trim(), RELEASE_PUBLIC_KEY);
    }

    #[test]
    fn is_newer_uses_semver_ordering() {
        let current = semver::Version::new(1, 2, 3);
        assert!(is_newer(&current, &semver::Version::new(1, 2, 4)));
        assert!(is_newer(&current, &semver::Version::new(2, 0, 0)));
        assert!(!is_newer(&current, &semver::Version::new(1, 2, 3)));
        assert!(!is_newer(&current, &semver::Version::new(1, 2, 2)));
    }

    #[tokio::test]
    async fn extract_tar_gz_unpacks_a_simple_archive() {
        let dir = std::env::temp_dir().join(format!("lanius-extract-test-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("test.tar.gz");

        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(5);
            header.set_path("hello.txt").unwrap();
            header.set_cksum();
            builder.append(&header, &b"world"[..]).unwrap();
            builder.finish().unwrap();
        }

        let dest_dir = dir.join("out");
        extract_tar_gz(&archive_path, &dest_dir).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("hello.txt")).unwrap(),
            "world"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn signed_bytes_len(bytes: &[u8]) -> u64 {
        bytes.len() as u64
    }

    fn uuid_like() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}-{:?}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::thread::current().id()
        )
    }
}
