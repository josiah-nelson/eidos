//! Release discovery and verified, bounded installer staging.
//!
//! This module deliberately stops before installation. It accepts only the
//! canonical setup asset of the latest release from the project's GitHub
//! repository, verifies its release digest and Windows publisher/product
//! metadata, and records durable progress under the application's data dir.

use crate::api::{blocking, ApiError, ApiResult};
use crate::api_json::ApiJson;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use eidos_domain::UnixNanos;
use parking_lot::Mutex;
use ring::digest::{Context as DigestContext, SHA256};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use ts_rs::TS;

const RELEASES_LATEST: &str = "https://api.github.com/repos/josiah-nelson/eidos/releases/latest";
const RELEASE_DOWNLOAD_PREFIX: &str = "https://github.com/josiah-nelson/eidos/releases/download/";
const UPDATES_DIR: &str = "updates";
const SETTINGS_FILE: &str = "settings.json";
const STATE_FILE: &str = "state.json";
const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
const HARD_MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(default)]
pub struct UpdateSettings {
    /// Automatic advisory checks. Manual checks remain available when false.
    pub automatic_checks: bool,
    /// Exact Authenticode certificate subject expected for release artifacts.
    pub expected_publisher: Option<String>,
    /// Exact Windows version-resource ProductName.
    pub expected_product: String,
    #[serde(with = "eidos_domain::json::u64_string")]
    #[ts(type = "ApiInt")]
    pub max_artifact_bytes: u64,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            automatic_checks: true,
            expected_publisher: None,
            expected_product: "Eidos".into(),
            max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
        }
    }
}

impl UpdateSettings {
    fn validated(mut self) -> anyhow::Result<Self> {
        self.expected_publisher = self
            .expected_publisher
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.expected_product = self.expected_product.trim().to_string();
        anyhow::ensure!(
            !self.expected_product.is_empty(),
            "expected product is required"
        );
        anyhow::ensure!(
            (1024 * 1024..=HARD_MAX_ARTIFACT_BYTES).contains(&self.max_artifact_bytes),
            "maximum artifact size must be between 1 MiB and 512 MiB"
        );
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ReleaseArtifact {
    pub version: String,
    pub name: String,
    pub download_url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum StagePhase {
    Idle,
    Downloading,
    Verifying,
    Staged,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct StagedArtifact {
    pub version: String,
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub publisher: String,
    pub product: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(default)]
pub struct UpdateState {
    pub checks_enabled: bool,
    pub current_version: String,
    pub checked_at: Option<UnixNanos>,
    pub check_error: Option<String>,
    pub available: Option<ReleaseArtifact>,
    pub stage_phase: StagePhase,
    pub stage_error: Option<String>,
    pub staged: Option<StagedArtifact>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            checks_enabled: true,
            current_version: env!("CARGO_PKG_VERSION").into(),
            checked_at: None,
            check_error: None,
            available: None,
            stage_phase: StagePhase::Idle,
            stage_error: None,
            staged: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}
#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
    state: String,
}

pub trait ArtifactVerifier: Send + Sync {
    fn verify(
        &self,
        path: &Path,
        version: &str,
        publisher: &str,
        product: &str,
    ) -> anyhow::Result<VerifiedIdentity>;
}

pub trait ReleaseSource: Send + Sync {
    fn check(&self, current: &str, max_bytes: u64) -> anyhow::Result<Option<ReleaseArtifact>>;
    fn download(
        &self,
        artifact: &ReleaseArtifact,
        path: &Path,
        max_bytes: u64,
    ) -> anyhow::Result<()>;
}

#[derive(Default)]
pub struct GithubReleaseSource;

impl ReleaseSource for GithubReleaseSource {
    fn check(&self, current: &str, max_bytes: u64) -> anyhow::Result<Option<ReleaseArtifact>> {
        fetch_release(RELEASES_LATEST, current, max_bytes, Duration::from_secs(15))
    }
    fn download(
        &self,
        artifact: &ReleaseArtifact,
        path: &Path,
        max_bytes: u64,
    ) -> anyhow::Result<()> {
        download_bounded(artifact, path, max_bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdentity {
    pub publisher: String,
    pub product: String,
}

#[derive(Default)]
pub struct SystemArtifactVerifier;

#[cfg(not(windows))]
impl ArtifactVerifier for SystemArtifactVerifier {
    fn verify(
        &self,
        _path: &Path,
        _version: &str,
        _publisher: &str,
        _product: &str,
    ) -> anyhow::Result<VerifiedIdentity> {
        anyhow::bail!("signed installer verification is supported only on Windows")
    }
}

#[cfg(windows)]
impl ArtifactVerifier for SystemArtifactVerifier {
    fn verify(
        &self,
        path: &Path,
        version: &str,
        publisher: &str,
        product: &str,
    ) -> anyhow::Result<VerifiedIdentity> {
        // Use Windows' Authenticode policy provider and version-resource reader
        // through the inbox PowerShell host. The program and script are fixed;
        // the downloaded path is passed as a separate argument.
        let system_root = std::env::var_os("SystemRoot")
            .ok_or_else(|| anyhow::anyhow!("SystemRoot is unavailable"))?;
        let powershell =
            PathBuf::from(system_root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let script = "$s=Get-AuthenticodeSignature -LiteralPath $args[0];$v=[Diagnostics.FileVersionInfo]::GetVersionInfo($args[0]);[pscustomobject]@{status=[string]$s.Status;publisher=if($s.SignerCertificate){$s.SignerCertificate.Subject}else{''};product=$v.ProductName;version=$v.ProductVersion}|ConvertTo-Json -Compress";
        let output = std::process::Command::new(powershell)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .arg(path)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Windows signature inspection failed"
        );
        #[derive(Deserialize)]
        struct Identity {
            status: String,
            publisher: String,
            product: String,
            version: String,
        }
        let got: Identity = serde_json::from_slice(&output.stdout)
            .map_err(|e| anyhow::anyhow!("invalid Windows signature response: {e}"))?;
        anyhow::ensure!(
            got.status == "Valid",
            "Authenticode trust failed: {}",
            got.status
        );
        anyhow::ensure!(
            got.publisher == publisher,
            "publisher mismatch: expected {publisher}, got {}",
            got.publisher
        );
        anyhow::ensure!(
            got.product == product,
            "product mismatch: expected {product}, got {}",
            got.product
        );
        anyhow::ensure!(
            normalize_version(&got.version).as_deref() == Some(version),
            "version mismatch: expected {version}, got {}",
            got.version
        );
        Ok(VerifiedIdentity {
            publisher: got.publisher,
            product: got.product,
        })
    }
}

pub struct UpdateManager {
    data_dir: PathBuf,
    settings: Mutex<UpdateSettings>,
    state: Mutex<UpdateState>,
    operation: Mutex<()>,
    verifier: Arc<dyn ArtifactVerifier>,
    source: Arc<dyn ReleaseSource>,
    command_line_checks: bool,
}

impl UpdateManager {
    pub fn load(data_dir: &Path, command_line_checks: bool) -> anyhow::Result<Self> {
        Self::load_with_adapters(
            data_dir,
            command_line_checks,
            Arc::new(SystemArtifactVerifier),
            Arc::new(GithubReleaseSource),
        )
    }

    pub fn load_with_adapters(
        data_dir: &Path,
        command_line_checks: bool,
        verifier: Arc<dyn ArtifactVerifier>,
        source: Arc<dyn ReleaseSource>,
    ) -> anyhow::Result<Self> {
        let dir = data_dir.join(UPDATES_DIR);
        let mut startup_error = None;
        let settings = match read_json::<UpdateSettings>(&dir.join(SETTINGS_FILE)) {
            Ok(Some(settings)) => settings.validated().unwrap_or_else(|error| {
                startup_error = Some(format!("update settings are invalid: {error:#}"));
                UpdateSettings {
                    automatic_checks: false,
                    ..UpdateSettings::default()
                }
            }),
            Ok(None) => UpdateSettings::default(),
            Err(error) => {
                startup_error = Some(format!("update settings are unreadable: {error:#}"));
                UpdateSettings {
                    automatic_checks: false,
                    ..UpdateSettings::default()
                }
            }
        };
        let mut state: UpdateState = match read_json(&dir.join(STATE_FILE)) {
            Ok(state) => state.unwrap_or_default(),
            Err(error) => {
                startup_error = Some(format!("update state was unreadable: {error:#}"));
                UpdateState::default()
            }
        };
        state.current_version = env!("CARGO_PKG_VERSION").into();
        state.checks_enabled = command_line_checks && settings.automatic_checks;
        if startup_error.is_some() {
            state.check_error = startup_error;
        }
        if state.available.as_ref().is_some_and(|artifact| {
            !version_is_compatible_newer(env!("CARGO_PKG_VERSION"), &artifact.version)
        }) {
            state.available = None;
        }
        if state.staged.as_ref().is_some_and(|artifact| {
            !version_is_compatible_newer(env!("CARGO_PKG_VERSION"), &artifact.version)
        }) {
            state.staged = None;
            state.stage_phase = StagePhase::Idle;
            state.stage_error = None;
        }
        if matches!(
            state.stage_phase,
            StagePhase::Downloading | StagePhase::Verifying
        ) {
            state.stage_phase = StagePhase::Failed;
            state.stage_error =
                Some("staging was interrupted; retry to download a fresh artifact".into());
        }
        store_json(&dir.join(STATE_FILE), &state)?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            settings: Mutex::new(settings),
            state: Mutex::new(state),
            operation: Mutex::new(()),
            verifier,
            source,
            command_line_checks,
        })
    }

    pub fn view(&self) -> UpdateState {
        self.state.lock().clone()
    }
    pub fn settings(&self) -> UpdateSettings {
        self.settings.lock().clone()
    }

    pub fn save_settings(&self, settings: UpdateSettings) -> anyhow::Result<UpdateState> {
        let _guard = self
            .operation
            .try_lock()
            .ok_or_else(|| anyhow::anyhow!("an update operation is already running"))?;
        let settings = settings.validated()?;
        store_json(
            &self.data_dir.join(UPDATES_DIR).join(SETTINGS_FILE),
            &settings,
        )?;
        *self.settings.lock() = settings.clone();
        let mut state = self.state.lock();
        state.checks_enabled = self.command_line_checks && settings.automatic_checks;
        self.store_state(&state)?;
        Ok(state.clone())
    }

    pub fn check(&self) -> anyhow::Result<UpdateState> {
        let _guard = self
            .operation
            .try_lock()
            .ok_or_else(|| anyhow::anyhow!("an update operation is already running"))?;
        let result = self.source.check(
            env!("CARGO_PKG_VERSION"),
            self.settings.lock().max_artifact_bytes,
        );
        let mut state = self.state.lock();
        state.checked_at = Some(UnixNanos::now());
        match result {
            Ok(available) => {
                state.available = available;
                state.check_error = None;
            }
            Err(error) => {
                state.check_error = Some(format!("{error:#}"));
                self.store_state(&state)?;
                return Err(error);
            }
        }
        self.store_state(&state)?;
        Ok(state.clone())
    }

    pub fn stage(&self) -> anyhow::Result<UpdateState> {
        let _guard = self
            .operation
            .try_lock()
            .ok_or_else(|| anyhow::anyhow!("an update operation is already running"))?;
        let settings = self.settings.lock().clone();
        let publisher = settings.expected_publisher.as_deref().ok_or_else(|| {
            anyhow::anyhow!("expected publisher must be configured before staging")
        })?;
        let artifact = self
            .state
            .lock()
            .available
            .clone()
            .ok_or_else(|| anyhow::anyhow!("check for a compatible release before staging"))?;
        validate_artifact(&artifact, settings.max_artifact_bytes)?;
        let dir = self.data_dir.join(UPDATES_DIR).join("staging");
        std::fs::create_dir_all(&dir)?;
        let partial = dir.join(format!("{}.part", artifact.name));
        let final_path = dir.join(&artifact.name);
        if partial.exists() {
            std::fs::remove_file(&partial)?;
        }
        self.set_phase(StagePhase::Downloading, None)?;
        let result = (|| {
            self.source
                .download(&artifact, &partial, settings.max_artifact_bytes)?;
            verify_staged_bytes(&partial, &artifact, settings.max_artifact_bytes)?;
            self.set_phase(StagePhase::Verifying, None)?;
            let identity = self.verifier.verify(
                &partial,
                &artifact.version,
                publisher,
                &settings.expected_product,
            )?;
            replace_file(&partial, &final_path)?;
            let mut state = self.state.lock();
            state.stage_phase = StagePhase::Staged;
            state.stage_error = None;
            state.staged = Some(StagedArtifact {
                version: artifact.version,
                path: final_path.display().to_string(),
                size: artifact.size,
                sha256: artifact.sha256,
                publisher: identity.publisher,
                product: identity.product,
            });
            self.store_state(&state)?;
            Ok(state.clone())
        })();
        if let Err(error) = &result {
            let _ = std::fs::remove_file(&partial);
            self.set_phase(StagePhase::Failed, Some(format!("{error:#}")))?;
        }
        result
    }

    fn set_phase(&self, phase: StagePhase, error: Option<String>) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        state.stage_phase = phase;
        state.stage_error = error;
        self.store_state(&state)
    }
    fn store_state(&self, state: &UpdateState) -> anyhow::Result<()> {
        store_json(&self.data_dir.join(UPDATES_DIR).join(STATE_FILE), state)
    }
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/updates", get(status))
        .route("/updates/settings", get(settings).post(save_settings))
        .route("/updates/check", post(check))
        .route("/updates/stage", post(stage))
}

async fn status(State(st): State<Arc<AppState>>) -> ApiResult<UpdateState> {
    Ok(ApiJson(st.updates.view()))
}

async fn settings(State(st): State<Arc<AppState>>) -> ApiResult<UpdateSettings> {
    Ok(ApiJson(st.updates.settings()))
}

async fn save_settings(
    State(st): State<Arc<AppState>>,
    Json(body): Json<UpdateSettings>,
) -> ApiResult<UpdateState> {
    blocking(move || {
        st.updates
            .save_settings(body)
            .map_err(|e| ApiError::bad_request(format!("{e:#}")))
    })
    .await
    .map(ApiJson)
}

async fn check(State(st): State<Arc<AppState>>) -> ApiResult<UpdateState> {
    blocking(move || {
        st.updates
            .check()
            .map_err(|e| ApiError::unavailable(format!("{e:#}"), Some(60)))
    })
    .await
    .map(ApiJson)
}

async fn stage(State(st): State<Arc<AppState>>) -> ApiResult<UpdateState> {
    blocking(move || {
        st.updates
            .stage()
            .map_err(|e| ApiError::bad_request(format!("{e:#}")))
    })
    .await
    .map(ApiJson)
}

fn normalize_version(value: &str) -> Option<String> {
    let pieces: Vec<_> = value.trim().trim_start_matches('v').split('.').collect();
    if pieces.len() < 3 || pieces.iter().any(|p| p.parse::<u64>().is_err()) {
        return None;
    }
    Some(format!("{}.{}.{}", pieces[0], pieces[1], pieces[2]))
}

fn version_is_compatible_newer(current: &str, candidate: &str) -> bool {
    let parse = |v: &str| -> Option<[u64; 3]> {
        let p: Vec<_> = v
            .trim()
            .trim_start_matches('v')
            .split('.')
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        (p.len() == 3).then(|| [p[0], p[1], p[2]])
    };
    matches!((parse(current), parse(candidate)), (Some(c), Some(n)) if n[0] == c[0] && n > c)
}

fn release_from_json(
    body: &[u8],
    current: &str,
    max: u64,
) -> anyhow::Result<Option<ReleaseArtifact>> {
    let release: GithubRelease = serde_json::from_slice(body)?;
    let version = release.tag_name.trim().trim_start_matches('v');
    if !version_is_compatible_newer(current, version) {
        return Ok(None);
    }
    let name = format!("eidos-v{version}-setup.exe");
    let Some(asset) = release
        .assets
        .into_iter()
        .find(|a| a.name == name && a.state == "uploaded")
    else {
        anyhow::bail!("release v{version} has no canonical setup asset")
    };
    anyhow::ensure!(
        asset.size > 0 && asset.size <= max,
        "release asset size is outside the configured bound"
    );
    let expected_url = format!("{RELEASE_DOWNLOAD_PREFIX}v{version}/{name}");
    anyhow::ensure!(
        asset.browser_download_url == expected_url,
        "release asset URL is not canonical"
    );
    let digest = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .ok_or_else(|| anyhow::anyhow!("release asset has no SHA-256 digest"))?;
    anyhow::ensure!(
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "release asset SHA-256 digest is malformed"
    );
    Ok(Some(ReleaseArtifact {
        version: version.into(),
        name,
        download_url: expected_url,
        size: asset.size,
        sha256: digest.to_ascii_lowercase(),
    }))
}

fn fetch_release(
    url: &str,
    current: &str,
    max: u64,
    timeout: Duration,
) -> anyhow::Result<Option<ReleaseArtifact>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .timeout_connect(Some(timeout.min(Duration::from_secs(5))))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .header("user-agent", concat!("eidos/", env!("CARGO_PKG_VERSION")))
        .header("accept", "application/vnd.github+json")
        .call()?;
    let body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_vec()?;
    release_from_json(&body, current, max)
}

fn download_bounded(artifact: &ReleaseArtifact, path: &Path, max: u64) -> anyhow::Result<()> {
    validate_artifact(artifact, max)?;
    anyhow::ensure!(
        artifact.download_url
            == format!(
                "{RELEASE_DOWNLOAD_PREFIX}v{}/{}",
                artifact.version, artifact.name
            ),
        "artifact URL is not allowlisted"
    );
    anyhow::ensure!(
        artifact.size <= max,
        "artifact exceeds configured size bound"
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .build()
        .into();
    let mut response = agent
        .get(&artifact.download_url)
        .header("user-agent", concat!("eidos/", env!("CARGO_PKG_VERSION")))
        .call()?;
    let mut reader = response.body_mut().as_reader();
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    let mut digest = DigestContext::new(&SHA256);
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| anyhow::anyhow!("artifact size overflow"))?;
        anyhow::ensure!(
            total <= max && total <= artifact.size,
            "artifact exceeded its declared size"
        );
        digest.update(&buffer[..count]);
        file.write_all(&buffer[..count])?;
    }
    file.sync_all()?;
    anyhow::ensure!(
        total == artifact.size,
        "artifact size mismatch: expected {}, got {total}",
        artifact.size
    );
    let got = digest
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    anyhow::ensure!(got == artifact.sha256, "artifact SHA-256 mismatch");
    Ok(())
}

fn validate_artifact(artifact: &ReleaseArtifact, max: u64) -> anyhow::Result<()> {
    anyhow::ensure!(
        version_is_compatible_newer(env!("CARGO_PKG_VERSION"), &artifact.version),
        "artifact version is not a compatible upgrade"
    );
    anyhow::ensure!(
        artifact.name == format!("eidos-v{}-setup.exe", artifact.version),
        "artifact name is not canonical"
    );
    anyhow::ensure!(
        artifact.size > 0 && artifact.size <= max,
        "artifact size is outside the configured bound"
    );
    anyhow::ensure!(
        artifact.sha256.len() == 64 && artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "artifact SHA-256 is malformed"
    );
    Ok(())
}

fn verify_staged_bytes(path: &Path, artifact: &ReleaseArtifact, max: u64) -> anyhow::Result<()> {
    let metadata = std::fs::metadata(path)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() == artifact.size && metadata.len() <= max,
        "staged artifact size does not match the bounded release asset"
    );
    let mut file = std::fs::File::open(path)?;
    let mut digest = DigestContext::new(&SHA256);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let got = digest
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    anyhow::ensure!(got == artifact.sha256, "staged artifact SHA-256 mismatch");
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn store_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("state path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    drop(file);
    replace_file(&tmp, path)
}
#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::rename(from, to)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    if !to.exists() {
        std::fs::rename(from, to)?;
        return Ok(());
    }
    let mut from_wide: Vec<u16> = from.as_os_str().encode_wide().collect();
    from_wide.push(0);
    let mut to_wide: Vec<u16> = to.as_os_str().encode_wide().collect();
    to_wide.push(0);
    // SAFETY: both buffers are owned, NUL-terminated, and live through the call.
    let replaced = unsafe {
        ReplaceFileW(
            to_wide.as_ptr(),
            from_wide.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_response_body_is_bounded_by_the_global_deadline() {
        use std::net::TcpListener;
        use std::time::Instant;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release, blocked) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = stream.read(&mut request).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{").unwrap();
            let _ = blocked.recv_timeout(Duration::from_secs(20));
        });
        let start = Instant::now();
        assert!(fetch_release(
            &format!("http://{address}"),
            "0.5.0",
            2_000_000,
            Duration::from_millis(150)
        )
        .is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn release_selection_is_exact_bounded_and_digest_backed() {
        let body = br#"{"tag_name":"v0.5.1","assets":[{"name":"eidos-v0.5.1-setup.exe","browser_download_url":"https://github.com/josiah-nelson/eidos/releases/download/v0.5.1/eidos-v0.5.1-setup.exe","size":1048576,"digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","state":"uploaded"}]}"#;
        let got = release_from_json(body, "0.5.0", 2_000_000)
            .unwrap()
            .unwrap();
        assert_eq!(got.version, "0.5.1");
        assert!(release_from_json(body, "0.5.1", 2_000_000)
            .unwrap()
            .is_none());
        assert!(release_from_json(body, "0.5.0", 100).is_err());
        let hostile = String::from_utf8(body.to_vec())
            .unwrap()
            .replace("github.com/josiah-nelson", "example.test/josiah-nelson");
        assert!(release_from_json(hostile.as_bytes(), "0.5.0", 2_000_000).is_err());
    }
    #[test]
    fn interrupted_stage_is_durably_failed_on_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state = UpdateState {
            stage_phase: StagePhase::Verifying,
            ..UpdateState::default()
        };
        store_json(&dir.path().join("updates/state.json"), &state).unwrap();
        let manager = UpdateManager::load_with_adapters(
            dir.path(),
            true,
            Arc::new(SystemArtifactVerifier),
            Arc::new(GithubReleaseSource),
        )
        .unwrap();
        assert_eq!(manager.view().stage_phase, StagePhase::Failed);
        assert!(manager.view().stage_error.unwrap().contains("interrupted"));
    }

    #[test]
    fn unreadable_settings_fail_updates_closed_without_blocking_service_open() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("updates")).unwrap();
        std::fs::write(dir.path().join("updates/settings.json"), b"{invalid").unwrap();
        let manager = UpdateManager::load_with_adapters(
            dir.path(),
            true,
            Arc::new(SystemArtifactVerifier),
            Arc::new(GithubReleaseSource),
        )
        .unwrap();
        assert!(!manager.view().checks_enabled);
        assert!(manager.view().check_error.unwrap().contains("unreadable"));
    }
    #[test]
    fn settings_require_bounds_and_product() {
        assert!(UpdateSettings {
            max_artifact_bytes: 1,
            ..Default::default()
        }
        .validated()
        .is_err());
        assert!(UpdateSettings {
            expected_product: " ".into(),
            ..Default::default()
        }
        .validated()
        .is_err());
        let wire: UpdateSettings = serde_json::from_str(
            r#"{"automatic_checks":true,"expected_publisher":null,"expected_product":"Eidos","max_artifact_bytes":"268435456"}"#,
        )
        .unwrap();
        assert_eq!(wire.max_artifact_bytes, DEFAULT_MAX_ARTIFACT_BYTES);
    }

    struct FixtureSource {
        artifact: ReleaseArtifact,
        bytes: Vec<u8>,
    }

    impl ReleaseSource for FixtureSource {
        fn check(&self, _current: &str, _max: u64) -> anyhow::Result<Option<ReleaseArtifact>> {
            Ok(Some(self.artifact.clone()))
        }
        fn download(
            &self,
            artifact: &ReleaseArtifact,
            path: &Path,
            max: u64,
        ) -> anyhow::Result<()> {
            anyhow::ensure!(
                self.bytes.len() as u64 <= max && self.bytes.len() as u64 == artifact.size,
                "fixture bound"
            );
            std::fs::write(path, &self.bytes)?;
            Ok(())
        }
    }

    struct FixtureVerifier;

    impl ArtifactVerifier for FixtureVerifier {
        fn verify(
            &self,
            _path: &Path,
            version: &str,
            publisher: &str,
            product: &str,
        ) -> anyhow::Result<VerifiedIdentity> {
            anyhow::ensure!(
                version == "0.5.1" && publisher == "CN=Test Publisher" && product == "Eidos",
                "identity mismatch"
            );
            Ok(VerifiedIdentity {
                publisher: publisher.into(),
                product: product.into(),
            })
        }
    }

    #[test]
    fn controlled_fixture_reaches_verified_durable_stage_without_execution() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"controlled setup fixture".to_vec();
        let digest = ring::digest::digest(&SHA256, &bytes)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let artifact = ReleaseArtifact {
            version: "0.5.1".into(),
            name: "eidos-v0.5.1-setup.exe".into(),
            download_url: format!("{RELEASE_DOWNLOAD_PREFIX}v0.5.1/eidos-v0.5.1-setup.exe"),
            size: bytes.len() as u64,
            sha256: digest,
        };
        let manager = UpdateManager::load_with_adapters(
            dir.path(),
            true,
            Arc::new(FixtureVerifier),
            Arc::new(FixtureSource { artifact, bytes }),
        )
        .unwrap();
        manager
            .save_settings(UpdateSettings {
                expected_publisher: Some("CN=Test Publisher".into()),
                ..Default::default()
            })
            .unwrap();
        manager.check().unwrap();
        let state = manager.stage().unwrap();
        assert_eq!(state.stage_phase, StagePhase::Staged);
        assert!(Path::new(&state.staged.unwrap().path).is_file());
    }
}
