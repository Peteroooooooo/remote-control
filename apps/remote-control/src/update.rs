use std::{
    ffi::OsString,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use rc_ident::{PublicKey, Signature};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const APP_VERSION: &str = env!("RC_APP_VERSION");

const GITHUB_REPO: &str = "Peteroooooooo/remote-control";
const CNB_REPO: &str = "peteroooooooo/remote-control";
const UPDATE_PUBLIC_KEY_HEX: &str = "9690e91c5328bd0ab4199ade23d4db93a66d37c9fc83a8baeb5efee017841686";

const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub version: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
    pub urls: Vec<String>,
    pub sig: String,
}

#[derive(Clone, Debug)]
pub enum CheckResult {
    UpToDate { current: String },
    Available(Manifest),
}

#[derive(Args)]
pub struct UpdateArgs {
    /// Only check for a new version without downloading or installing.
    #[arg(long)]
    pub check: bool,
    /// Download and install without asking for confirmation.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Args)]
pub struct ApplyUpdateArgs {
    #[arg(long)]
    pub target: PathBuf,
    #[arg(long, default_value_t = 0)]
    pub pid: u32,
    #[arg(long)]
    pub service: bool,
    #[arg(long)]
    pub open_lobby: bool,
    #[arg(long)]
    pub elevated: bool,
}

pub fn manifest_urls() -> Vec<String> {
    let mut urls = Vec::with_capacity(2);
    if !CNB_REPO.is_empty() {
        urls.push(format!("https://cnb.cool/{CNB_REPO}/-/releases/download/latest/latest.json"));
    }
    urls.push(format!("https://github.com/{GITHUB_REPO}/releases/latest/download/latest.json"));
    urls
}

fn canonical_payload(version: &str, sha256: &str) -> Vec<u8> {
    let v = version.trim().trim_start_matches('v');
    let h = sha256.trim().to_ascii_lowercase();
    format!("remote-control-update-v1\n{v}\n{h}\n").into_bytes()
}

fn decode_hex(s: &str, out: &mut [u8]) -> Result<()> {
    let s = s.trim();
    if s.len() != out.len() * 2 {
        bail!("unexpected hex length: expected {}, got {}", out.len() * 2, s.len());
    }
    for (i, slot) in out.iter_mut().enumerate() {
        let pair = &s[i * 2..i * 2 + 2];
        *slot = u8::from_str_radix(pair, 16).with_context(|| format!("invalid hex at byte {i}"))?;
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: bool,
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor, self.patch, !self.prerelease)
            .cmp(&(other.major, other.minor, other.patch, !other.prerelease))
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_semver(raw: &str) -> Option<SemVer> {
    let s = raw.trim().trim_start_matches('v');
    let s = s.split('+').next()?;
    let (core, prerelease) = match s.split_once('-') {
        Some((c, _)) => (c, true),
        None => (s, false),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(SemVer { major, minor, patch, prerelease })
}

pub fn is_newer(remote: &str, current: &str) -> bool {
    match (parse_semver(remote), parse_semver(current)) {
        (Some(r), Some(c)) => r > c,
        _ => false,
    }
}

pub fn verify_manifest(manifest: &Manifest) -> Result<()> {
    verify_manifest_with_key(manifest, UPDATE_PUBLIC_KEY_HEX)
}

fn verify_manifest_with_key(manifest: &Manifest, pubkey_hex: &str) -> Result<()> {
    if parse_semver(&manifest.version).is_none() {
        bail!("invalid version in manifest: {}", manifest.version);
    }
    let mut sha = [0u8; 32];
    decode_hex(&manifest.sha256, &mut sha).context("invalid sha256 in manifest")?;

    if !manifest.urls.iter().any(|u| u.starts_with("https://")) {
        bail!("manifest has no https download url");
    }

    let mut pk_bytes = [0u8; 32];
    decode_hex(pubkey_hex, &mut pk_bytes).context("invalid update public key")?;
    let pk = PublicKey::from_bytes(&pk_bytes).map_err(|e| anyhow!("invalid public key: {e}"))?;

    let mut sig_bytes = [0u8; 64];
    decode_hex(&manifest.sig, &mut sig_bytes).context("invalid manifest signature format")?;
    let sig = Signature::from_bytes(&sig_bytes);

    let msg = canonical_payload(&manifest.version, &manifest.sha256);
    pk.verify(&msg, &sig).map_err(|_| anyhow!("manifest signature verification failed"))?;
    Ok(())
}

fn http_client(timeout: Duration) -> Result<reqwest::Client> {
    let tls = rc_relay::tls::ca()
        .client_config(iroh_relay::tls::default_provider())
        .context("TLS setup")?;
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .user_agent(format!("remote-control/{APP_VERSION}"))
        .connect_timeout(Duration::from_secs(8))
        .timeout(timeout)
        .build()
        .context("HTTP client")
}

async fn fetch_manifest(client: &reqwest::Client, url: &str) -> Result<Manifest> {
    let resp = client.get(url).send().await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    let manifest: Manifest = serde_json::from_slice(&bytes).context("parse manifest json")?;
    verify_manifest(&manifest)?;
    Ok(manifest)
}

pub async fn check() -> Result<CheckResult> {
    let client = http_client(CHECK_TIMEOUT)?;
    let urls = manifest_urls();
    let mut set = tokio::task::JoinSet::new();
    for url in urls {
        let c = client.clone();
        set.spawn(async move {
            let r = fetch_manifest(&c, &url).await;
            (url, r)
        });
    }

    let mut errors = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((_url, Ok(manifest))) => {
                set.abort_all();
                let ver = manifest.version.trim().trim_start_matches('v').to_string();
                if is_newer(&ver, APP_VERSION) {
                    return Ok(CheckResult::Available(manifest));
                }
                return Ok(CheckResult::UpToDate { current: APP_VERSION.to_string() });
            }
            Ok((url, Err(e))) => errors.push(format!("{url}: {e:#}")),
            Err(e) => errors.push(format!("task failed: {e}")),
        }
    }

    bail!("all update sources failed: {}", errors.join("; "))
}

pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = OsString::from(path.as_os_str());
    s.push(suffix);
    PathBuf::from(s)
}

pub async fn download_and_stage(
    manifest: &Manifest,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    verify_manifest(manifest)?;
    let exe = crate::install::live_exe().context("locate current executable")?;
    let stage = with_suffix(&exe, ".new");
    let client = http_client(DOWNLOAD_TIMEOUT)?;

    let mut errors = Vec::new();
    for url in manifest.urls.iter().filter(|u| u.starts_with("https://")) {
        let _ = std::fs::remove_file(&stage);
        match download_one(&client, url, &stage, &manifest.sha256, &mut on_progress).await {
            Ok(()) => return Ok(stage),
            Err(e) => {
                let _ = std::fs::remove_file(&stage);
                errors.push(format!("{url}: {e:#}"));
            }
        }
    }

    bail!("download failed from all sources: {}", errors.join("; "))
}

async fn download_one(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    on_progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let mut resp = client.get(url).send().await?.error_for_status()?;
    let total = resp.content_length();
    let mut file = std::fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    on_progress(0, total);

    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk)?;
        hasher.update(&chunk);
        downloaded += chunk.len() as u64;
        on_progress(downloaded, total);
    }
    file.flush()?;
    drop(file);

    let actual = encode_hex(&hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        bail!("sha256 mismatch: expected {}, got {}", expected_sha256.trim(), actual);
    }
    Ok(())
}

pub fn service_uses_current_exe() -> bool {
    let Some(svc_exe) = crate::install::service_exe() else {
        return false;
    };
    let Ok(me) = crate::install::live_exe() else {
        return false;
    };
    crate::install::same_file(&me, &svc_exe)
}

pub fn launch_apply_and_exit(stage: &Path, service: bool, open_lobby: bool) -> Result<()> {
    let target = crate::install::live_exe().context("locate current executable")?;
    let pid = std::process::id();
    let mut cmd = Command::new(stage);
    cmd.arg("apply-update").arg("--target").arg(&target).arg("--pid").arg(pid.to_string());
    if service {
        cmd.arg("--service");
    }
    if open_lobby {
        cmd.arg("--open-lobby");
    }
    cmd.spawn().with_context(|| format!("launch {}", stage.display()))?;
    crate::goaway::done();
    std::process::exit(0);
}

fn wait_pid_exit(pid: u32, timeout_ms: u32) {
    if pid == 0 {
        return;
    }
    use windows::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(h, timeout_ms);
            let _ = CloseHandle(h);
        }
    }
}

fn move_aside_old(target: &Path) -> io::Result<()> {
    for idx in 0..5 {
        let suffix = if idx == 0 { ".old".to_string() } else { format!(".old.{idx}") };
        let old = with_suffix(target, &suffix);
        if old.exists() && std::fs::remove_file(&old).is_err() {
            continue;
        }
        return std::fs::rename(target, &old);
    }
    let old = with_suffix(target, ".old");
    std::fs::rename(target, &old)
}

fn run_elevated_apply(target: &Path, service: bool) -> Result<()> {
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
            UI::{
                Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
                WindowsAndMessaging::SW_HIDE,
            },
        },
        core::{HSTRING, w},
    };
    let self_exe = crate::install::live_exe()?;
    let mut params = format!("apply-update --target \"{}\" --pid 0 --elevated", target.display());
    if service {
        params.push_str(" --service");
    }
    let (file, params) = (HSTRING::from(self_exe.as_os_str()), HSTRING::from(params));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        lpParameters: windows::core::PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    let code = unsafe {
        ShellExecuteExW(&mut info).map_err(|e| anyhow!("UAC elevation failed: {e}"))?;
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        code
    };
    if code != 0 {
        bail!("elevated update exited with code {code}");
    }
    Ok(())
}

pub fn apply_update(args: ApplyUpdateArgs) -> Result<()> {
    wait_pid_exit(args.pid, 15_000);

    if args.service {
        if let Err(e) = crate::install::stop_service_now() {
            if !args.elevated {
                run_elevated_apply(&args.target, true)?;
                if args.open_lobby {
                    Command::new(&args.target).spawn().context("relaunch lobby")?;
                }
                return Ok(());
            }
            return Err(e).context("stop service for update");
        }
        if args.elevated {
            crate::install::kill_helpers_for(&args.target);
        }
    }

    let self_exe = crate::install::live_exe().context("locate staged executable")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut moved = false;
    let mut last_err = None;

    if !args.target.exists() {
        moved = true;
    } else {
        loop {
            match move_aside_old(&args.target) {
                Ok(()) => {
                    moved = true;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied && !args.elevated => {
                    last_err = Some(e);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }

    if !moved {
        if !args.elevated
            && last_err
                .as_ref()
                .is_some_and(|e| e.kind() == io::ErrorKind::PermissionDenied)
        {
            run_elevated_apply(&args.target, args.service)?;
            if args.open_lobby {
                Command::new(&args.target).spawn().context("relaunch lobby")?;
            }
            return Ok(());
        }
        let e = last_err.unwrap_or_else(|| io::Error::other("unknown rename failure"));
        return Err(e).with_context(|| format!("replace {}", args.target.display()));
    }

    let tmp = with_suffix(&args.target, ".tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::copy(&self_exe, &tmp).with_context(|| format!("copy update to {}", tmp.display()))?;
    std::fs::rename(&tmp, &args.target).with_context(|| format!("install {}", args.target.display()))?;

    if args.service {
        crate::install::start_service().context("restart service after update")?;
    }
    if args.open_lobby {
        Command::new(&args.target).spawn().context("relaunch lobby")?;
    }
    Ok(())
}

pub fn cleanup_stale_files() {
    let Ok(exe) = crate::install::live_exe() else {
        return;
    };
    let name = exe.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with(".new") {
        return;
    }
    for suffix in [".new", ".tmp", ".old", ".old.1", ".old.2", ".old.3", ".old.4"] {
        let candidate = with_suffix(&exe, suffix);
        if candidate.exists() {
            let _ = std::fs::remove_file(candidate);
        }
    }
}

pub fn run_cli(args: UpdateArgs) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    println!("Current version: v{APP_VERSION}");
    println!("Checking for updates...");

    match rt.block_on(check())? {
        CheckResult::UpToDate { current } => {
            println!("Already up to date (v{current}).");
            Ok(())
        }
        CheckResult::Available(manifest) => {
            let next = manifest.version.trim().trim_start_matches('v');
            println!("New version available: v{next}");
            if !manifest.notes.trim().is_empty() {
                println!("{}", manifest.notes.trim());
            }
            if args.check {
                return Ok(());
            }
            if !args.yes {
                print!("Download and install v{next}? [y/N]: ");
                io::stdout().flush()?;
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
                let ok = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
                if !ok {
                    println!("Cancelled.");
                    return Ok(());
                }
            }

            let mut last_pct = 200u64;
            let stage = rt.block_on(download_and_stage(&manifest, |done, total| {
                if let Some(t) = total.filter(|&v| v > 0) {
                    let pct = done.saturating_mul(100) / t;
                    if pct >= last_pct.wrapping_add(10) || last_pct == 200 || done == t {
                        last_pct = pct;
                        println!("Downloading... {pct}% ({:.1} / {:.1} MB)", done as f64 / 1e6, t as f64 / 1e6);
                    }
                }
            }))?;

            let service = service_uses_current_exe();
            let target = crate::install::live_exe()?;
            println!("Applying update...");
            let mut cmd = Command::new(&stage);
            cmd.arg("apply-update").arg("--target").arg(&target).arg("--pid").arg("0");
            if service {
                cmd.arg("--service");
            }
            let status = cmd.status().with_context(|| format!("run {}", stage.display()))?;
            if !status.success() {
                bail!("update helper exited with {status}");
            }
            println!("Updated to v{next}.");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_ordering() {
        assert!(is_newer("0.3.1", "0.3.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.3.1", "0.3.1-rc1"));
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.2.9", "0.3.0"));
        assert!(!is_newer("0.3.1-rc1", "0.3.1"));
    }

    #[test]
    fn verifies_node_ed25519_signature() {
        let m = Manifest {
            version: "0.3.1".into(),
            sha256: "0".repeat(64),
            notes: "test".into(),
            urls: vec!["https://github.com/Peteroooooooo/remote-control/releases/download/v0.3.1/remote-control.exe".into()],
            sig: "fc107aa84f23e17d30fa3bdf4088801a508d40269e7465e2eb3ce6aa70f24c5319a42156cd07255f733fe97c20832466d13a9c8feb9381510362f2076f27ae0c".into(),
        };
        verify_manifest_with_key(&m, UPDATE_PUBLIC_KEY_HEX).expect("valid signature");

        let mut bad = m.clone();
        bad.version = "0.3.2".into();
        assert!(verify_manifest_with_key(&bad, UPDATE_PUBLIC_KEY_HEX).is_err());

        let mut http_only = m.clone();
        http_only.urls = vec!["http://example.com/rc.exe".into()];
        assert!(verify_manifest_with_key(&http_only, UPDATE_PUBLIC_KEY_HEX).is_err());
    }
}
