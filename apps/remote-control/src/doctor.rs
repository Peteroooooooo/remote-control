//! One-click ("wheelchair") network diagnostics and guided / one-click fixes
//! for Windows (`remote-control doctor` and Lobby Settings → Network Check).
//!
//! Strictly respects the user's relay settings: never contacts public relays
//! when `public_relays` is disabled.

use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    os::windows::process::CommandExt,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand};
use iroh::{
    Endpoint, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey, Watcher,
    endpoint::presets,
    unstable_net_report::{NetReport, Probe},
};
use netdev::interface::types::InterfaceType;
use winreg::{RegKey, enums::*};

use crate::{
    i18n::{is_en, tr},
    install, net, store,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const FIREWALL_ALLOW_RULE: &str = "remote-control-allow";
const CLASH_DIRECT_RULE: &str = "PROCESS-NAME,remote-control.exe,DIRECT";
const CAPTIVE_PORTAL_URL: &str = "http://captive.apple.com/hotspot-detect.html";

#[derive(Clone, Debug)]
pub struct DoctorInput {
    pub public_relays: bool,
    pub lan_visible: bool,
    /// Configured self-hosted relays; each inner Vec holds one relay's URLs (e.g. v4 + v6).
    pub own_relays: Vec<Vec<RelayUrl>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Err,
    Warn,
    Ok,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FixAction {
    /// Toggle app setting without elevation and re-test.
    EnablePublicRelays,
    EnableLanDiscovery,
    /// Elevated system fix (`fix-net`), then auto re-test.
    EnableIpv6 {
        adapter_name: Option<String>,
        fix_registry: bool,
    },
    AllowFirewall,
    SetPrivateNetwork,
    /// External / guided actions.
    CopyText {
        label_zh: &'static str,
        label_en: &'static str,
        text: String,
    },
    OpenUrl {
        label_zh: &'static str,
        label_en: &'static str,
        url: String,
    },
    OpenNetworkAdapters,
    OpenNetworkSettings,
    JumpRelays,
}

impl FixAction {
    pub fn label(&self) -> &'static str {
        match self {
            FixAction::EnablePublicRelays => tr("开启公网辅助连接", "Enable Public Assist"),
            FixAction::EnableLanDiscovery => tr("开启局域网发现", "Enable LAN Discovery"),
            FixAction::EnableIpv6 { .. } => tr("一键开启 IPv6", "Enable IPv6"),
            FixAction::AllowFirewall => tr("一键放行防火墙", "Allow in Firewall"),
            FixAction::SetPrivateNetwork => tr("一键切为专用网络", "Switch to Private Network"),
            FixAction::CopyText { label_zh, label_en, .. } => tr(label_zh, label_en),
            FixAction::OpenUrl { label_zh, label_en, .. } => tr(label_zh, label_en),
            FixAction::OpenNetworkAdapters => tr("打开网络连接", "Open Network Adapters"),
            FixAction::OpenNetworkSettings => tr("打开网络设置", "Open Network Settings"),
            FixAction::JumpRelays => tr("去配置自建中转", "Configure Private Relay"),
        }
    }

    pub fn needs_admin(&self) -> bool {
        matches!(
            self,
            FixAction::EnableIpv6 { .. } | FixAction::AllowFirewall | FixAction::SetPrivateNetwork
        )
    }
}

#[derive(Clone, Debug)]
pub struct CheckItem {
    #[cfg_attr(not(test), allow(dead_code))]
    pub id: &'static str,
    pub level: Level,
    pub title: String,
    pub detail: String,
    pub guidance: String,
    pub actions: Vec<FixAction>,
}

#[derive(Clone, Debug)]
pub struct DoctorReport {
    pub level: Level,
    pub headline: String,
    pub summary: String,
    pub items: Vec<CheckItem>,
    pub raw_lines: Vec<String>,
    pub elapsed_ms: u64,
}

impl DoctorReport {
    /// Clean plain-text report suitable for clipboard sharing or CLI output.
    /// Never includes device keys or relay certificate pins / auth tokens.
    pub fn to_plain_text(&self) -> String {
        let mut out = String::new();
        let icon = match self.level {
            Level::Ok => "[OK]",
            Level::Warn => "[WARN]",
            Level::Err => "[ERR]",
            Level::Info => "[INFO]",
        };
        out.push_str(&format!(
            "Remote Control {} — {} ({} ms)\n",
            crate::update::APP_VERSION,
            tr("网络诊断报告", "Network Diagnostics"),
            self.elapsed_ms
        ));
        out.push_str(&format!("{icon} {}\n", self.headline));
        if !self.summary.is_empty() {
            out.push_str(&format!("  {}\n", self.summary));
        }
        out.push('\n');
        for item in &self.items {
            let tag = match item.level {
                Level::Ok => "[OK]  ",
                Level::Warn => "[WARN]",
                Level::Err => "[ERR] ",
                Level::Info => "[INFO]",
            };
            out.push_str(&format!("{tag} {}\n", item.title));
            if !item.detail.is_empty() {
                out.push_str(&format!("       {}\n", item.detail));
            }
            if !item.guidance.is_empty() {
                out.push_str(&format!("       {} {}\n", tr("建议:", "Fix:"), item.guidance));
            }
        }
        if !self.raw_lines.is_empty() {
            out.push_str(&format!("\n--- {} ---\n", tr("技术详情", "Details")));
            for line in &self.raw_lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }
}

/// CLI `remote-control doctor`.
#[derive(Args)]
pub struct DoctorArgs {
    /// Device key path (used to locate settings.txt; default: this user's).
    #[arg(long)]
    pub key: Option<PathBuf>,
}

/// Hidden elevated helper: `remote-control fix-net ...`.
#[derive(Args)]
pub struct FixNetArgs {
    /// File path to write status/error output for the lobby.
    #[arg(long)]
    pub report: Option<PathBuf>,
    #[command(subcommand)]
    pub action: FixNetCmd,
}

#[derive(Subcommand)]
pub enum FixNetCmd {
    AllowFirewall,
    EnableIpv6 {
        #[arg(long)]
        adapter: Option<String>,
        #[arg(long)]
        fix_registry: bool,
    },
    SetPrivateNetwork,
}

/// Runs `remote-control doctor` from CLI and prints the bilingual report.
pub fn run_cli(args: DoctorArgs) -> Result<()> {
    let key = args.key.unwrap_or_else(net::default_key_path);
    let settings = store::Settings::load(&store::settings_path(&key));
    crate::i18n::set_pref(settings.language);
    println!(
        "{}",
        tr(
            "正在检测网络环境，请稍候（约 3~5 秒）…",
            "Checking network environment (3–5 s)…"
        )
    );
    let input = DoctorInput {
        public_relays: settings.public_relays,
        lan_visible: settings.lan_visible,
        own_relays: settings.relays,
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let report = rt.block_on(diagnose(input));
    println!("\n{}", report.to_plain_text());
    Ok(())
}

/// Executes a whitelisted elevated system fix (`remote-control fix-net ...`).
pub fn run_fix_net(args: FixNetArgs) -> Result<()> {
    let res = match args.action {
        FixNetCmd::AllowFirewall => fix_allow_firewall(),
        FixNetCmd::EnableIpv6 { adapter, fix_registry } => {
            fix_enable_ipv6(adapter.as_deref(), fix_registry)
        }
        FixNetCmd::SetPrivateNetwork => fix_set_private_network(),
    };
    if let Some(path) = args.report {
        let text = match &res {
            Ok(msg) => format!("OK: {msg}\n"),
            Err(e) => format!("Error: {e:#}\n"),
        };
        let _ = std::fs::write(&path, text);
    }
    res.map(|_| ())
}

/// Launches `remote-control fix-net` elevated via UAC and returns the result message.
pub fn run_fix_elevated(action: &FixAction) -> Result<String> {
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
    let exe = install::live_exe().or_else(|_| std::env::current_exe().map_err(anyhow::Error::from))?;
    let report = std::env::temp_dir().join(format!("rc-fix-net-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&report);

    let sub = match action {
        FixAction::AllowFirewall => "allow-firewall".to_string(),
        FixAction::SetPrivateNetwork => "set-private-network".to_string(),
        FixAction::EnableIpv6 { adapter_name, fix_registry } => {
            let mut s = "enable-ipv6".to_string();
            if let Some(name) = adapter_name {
                let escaped = name.replace('"', "\\\"");
                s.push_str(&format!(" --adapter \"{escaped}\""));
            }
            if *fix_registry {
                s.push_str(" --fix-registry");
            }
            s
        }
        _ => return Err(anyhow!("not an elevated fix")),
    };
    let params = format!("fix-net --report \"{}\" {sub}", report.display());
    let (file_h, params_h) = (HSTRING::from(exe.as_os_str()), HSTRING::from(params));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: windows::core::PCWSTR(file_h.as_ptr()),
        lpParameters: windows::core::PCWSTR(params_h.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    let code = unsafe {
        ShellExecuteExW(&mut info).map_err(|e: windows::core::Error| {
            if e.code().0 as u32 == 0x8007_04C7 {
                anyhow!("{}", tr("管理员确认已取消", "Administrator prompt was cancelled"))
            } else {
                anyhow!("{e}")
            }
        })?;
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 1u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        code
    };
    let out = std::fs::read_to_string(&report).unwrap_or_default();
    let _ = std::fs::remove_file(&report);
    if code != 0 {
        let why = out
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix("Error: "))
            .unwrap_or_else(|| tr("操作失败，请重试", "Operation failed"));
        return Err(anyhow!("{why}"));
    }
    let ok_msg = out
        .lines()
        .find_map(|l| l.strip_prefix("OK: "))
        .unwrap_or_else(|| tr("修复完成", "Fixed"));
    Ok(ok_msg.to_string())
}

/// Safely opens an http/https URL, `ms-settings:...`, or `ncpa.cpl` via ShellExecuteW.
pub fn open_shell_target(target: &str) -> Result<()> {
    let allowed = target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("ms-settings:")
        || target == "ncpa.cpl";
    if !allowed {
        return Err(anyhow!("disallowed target"));
    }
    use windows::{
        Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        core::{HSTRING, w},
    };
    let h = HSTRING::from(target);
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            windows::core::PCWSTR(h.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if (r.0 as isize) <= 32 {
        return Err(anyhow!("ShellExecuteW failed ({})", r.0 as isize));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Local inspection structures
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct AdapterSummary {
    friendly_name: String,
    description: String,
    ipv4s: Vec<Ipv4Addr>,
    has_link_local_v6: bool,
    global_v6s: Vec<Ipv6Addr>,
    gateway_v4: Option<Ipv4Addr>,
    is_tun: bool,
    is_vlan: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FirewallState {
    Disabled,
    Allowed,
    MissingAllow,
    Blocked,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NetProfileState {
    PrivateOrDomain,
    Public,
    Unknown,
}

#[derive(Clone, Debug)]
struct LocalEnv {
    egress_v4: Option<Ipv4Addr>,
    egress_adapter: Option<AdapterSummary>,
    primary_physical: Option<AdapterSummary>,
    all_local_v4: Vec<Ipv4Addr>,
    all_global_v6: Vec<Ipv6Addr>,
    virtual_lans: Vec<String>,
    tun_adapter_name: Option<String>,
    ipv6_reg_disabled: bool,
    ipv6_detour: Option<Ipv6Addr>,
    system_proxy: Option<String>,
    firewall: FirewallState,
    net_profile: NetProfileState,
}

#[derive(Clone, Debug)]
struct OwnRelayProbe {
    display_host: String,
    https_ms: Option<u64>,
    qad_v4_ms: Option<u64>,
    qad_v6_ms: Option<u64>,
    https_err: Option<String>,
    /// True when the relay resolves to a LAN/private IP (e.g. home Pi on 192.168.x.x).
    is_lan_relay: bool,
    /// True when the relay only resolves to IPv6 while local IPv6 UDP was disabled due to detour.
    v6_only_skipped_udp: bool,
}

/// Runs the full local + network diagnostic suite.
pub async fn diagnose(input: DoctorInput) -> DoctorReport {
    let started = Instant::now();
    let local = tokio::task::spawn_blocking(inspect_local_env)
        .await
        .unwrap_or_else(|_| LocalEnv {
            egress_v4: None,
            egress_adapter: None,
            primary_physical: None,
            all_local_v4: Vec::new(),
            all_global_v6: Vec::new(),
            virtual_lans: Vec::new(),
            tun_adapter_name: None,
            ipv6_reg_disabled: false,
            ipv6_detour: None,
            system_proxy: None,
            firewall: FirewallState::Unknown,
            net_profile: NetProfileState::Unknown,
        });

    // Build probe URLs strictly honoring `input.public_relays`.
    let mut own_flat: Vec<RelayUrl> = Vec::new();
    for group in &input.own_relays {
        rc_relay::tls::learn_all(group);
        for u in group {
            if !own_flat.contains(u) {
                own_flat.push(u.clone());
            }
        }
    }
    let pub_urls: Vec<RelayUrl> = if input.public_relays {
        net::public_relays()
    } else {
        Vec::new()
    };
    let mut all_probe_urls = own_flat.clone();
    for u in &pub_urls {
        if !all_probe_urls.contains(u) {
            all_probe_urls.push(u.clone());
        }
    }

    let distinct_hosts: HashSet<String> = all_probe_urls
        .iter()
        .filter_map(|u| u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase()))
        .collect();
    let distinct_host_count = distinct_hosts.len();

    let (fake_ip, net_report) = tokio::join!(
        check_fake_ip(&all_probe_urls),
        async {
            if !all_probe_urls.is_empty() {
                run_iroh_net_report(&all_probe_urls, local.ipv6_detour.is_some()).await
            } else {
                None
            }
        }
    );

    // Per-own-relay detailed status (combining net_report latencies + direct /ping error diagnosis).
    let mut own_probes: Vec<OwnRelayProbe> = Vec::new();
    for group in &input.own_relays {
        if group.is_empty() {
            continue;
        }
        let display_host = group.first().map(clean_relay_label).unwrap_or_default();
        let (mut https_ms, mut qad_v4_ms, mut qad_v6_ms) = (None, None, None);
        if let Some(rep) = &net_report {
            for (probe, url, dur) in rep.relay_latency.iter() {
                if group.contains(url) {
                    let ms = dur.as_millis().max(1) as u64;
                    match probe {
                        Probe::Https => https_ms = Some(https_ms.map_or(ms, |old: u64| old.min(ms))),
                        Probe::QadIpv4 => qad_v4_ms = Some(qad_v4_ms.map_or(ms, |old: u64| old.min(ms))),
                        Probe::QadIpv6 => qad_v6_ms = Some(qad_v6_ms.map_or(ms, |old: u64| old.min(ms))),
                        _ => {}
                    }
                }
            }
        }
        let mut https_err = None;
        if https_ms.is_none() {
            match probe_own_relay_https(group).await {
                Ok(ms) => https_ms = Some(ms),
                Err(e) => https_err = Some(e),
            }
        }
        let need_addr_inspect = https_ms.is_some() && qad_v4_ms.is_none() && qad_v6_ms.is_none();
        let (is_lan_relay, has_ipv4) = if need_addr_inspect {
            inspect_relay_group_addrs(group).await
        } else {
            (false, true)
        };
        let v6_only_skipped_udp =
            need_addr_inspect && !is_lan_relay && local.ipv6_detour.is_some() && !has_ipv4;
        own_probes.push(OwnRelayProbe {
            display_host,
            https_ms,
            qad_v4_ms,
            qad_v6_ms,
            https_err,
            is_lan_relay,
            v6_only_skipped_udp,
        });
    }

    // Best public relay latency from net_report.
    let mut pub_https_best: Option<(String, u64)> = None;
    let mut pub_qad_best: Option<u64> = None;
    if let Some(rep) = &net_report
        && !pub_urls.is_empty()
    {
        for (probe, url, dur) in rep.relay_latency.iter() {
            if pub_urls.contains(url) {
                let ms = dur.as_millis().max(1) as u64;
                match probe {
                    Probe::Https => {
                        if pub_https_best.as_ref().is_none_or(|(_, b)| ms < *b) {
                            pub_https_best = Some((clean_relay_label(url), ms));
                        }
                    }
                    Probe::QadIpv4 | Probe::QadIpv6 => {
                        pub_qad_best = Some(pub_qad_best.map_or(ms, |b| b.min(ms)));
                    }
                    _ => {}
                }
            }
        }
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    build_report(
        &input,
        &local,
        fake_ip,
        net_report.as_ref(),
        distinct_host_count,
        &own_probes,
        pub_https_best,
        pub_qad_best,
        elapsed_ms,
    )
}

async fn run_iroh_net_report(urls: &[RelayUrl], disable_v6: bool) -> Option<NetReport> {
    let map = RelayMap::from_iter(urls.iter().cloned().map(RelayConfig::from));
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::generate())
        .ca_tls_config(rc_relay::tls::ca())
        .relay_mode(RelayMode::Custom(map));
    if disable_v6 {
        builder = builder
            .clear_ip_transports()
            .bind_addr("0.0.0.0:0".parse::<SocketAddr>().expect("valid"))
            .expect("valid bind address");
    }
    let ep = tokio::time::timeout(Duration::from_secs(3), builder.bind())
        .await
        .ok()?
        .ok()?;
    let report = tokio::time::timeout(Duration::from_secs(7), ep.net_report().initialized())
        .await
        .ok();
    let _ = tokio::time::timeout(Duration::from_secs(1), ep.close()).await;
    report
}

/// Inspects resolved addresses of a relay group: returns `(is_lan_relay, has_ipv4)`.
async fn inspect_relay_group_addrs(urls: &[RelayUrl]) -> (bool, bool) {
    let mut saw_any = false;
    let mut all_lan = true;
    let mut has_v4 = false;
    let mut check_ip = |ip: IpAddr| {
        saw_any = true;
        match ip {
            IpAddr::V4(v4) => {
                has_v4 = true;
                if !(v4.is_private() || v4.is_loopback() || v4.is_link_local()) {
                    all_lan = false;
                }
            }
            IpAddr::V6(v6) => {
                let is_ula = (v6.segments()[0] & 0xfe00) == 0xfc00;
                if !(is_ula || is_link_local_v6(v6) || v6.is_loopback()) {
                    all_lan = false;
                }
            }
        }
    };
    for u in urls {
        let Some(host) = u.host_str() else { continue };
        let host = host.trim_end_matches('.');
        if let Ok(v4) = host.parse::<Ipv4Addr>() {
            check_ip(IpAddr::V4(v4));
            continue;
        }
        if let Ok(v6) = host.trim_matches(['[', ']']).parse::<Ipv6Addr>() {
            check_ip(IpAddr::V6(v6));
            continue;
        }
        let port = u.port().unwrap_or(443);
        if let Ok(Ok(addrs)) =
            tokio::time::timeout(Duration::from_secs(2), tokio::net::lookup_host((host, port))).await
        {
            for a in addrs {
                check_ip(a.ip());
            }
        }
    }
    (saw_any && all_lan, has_v4)
}

async fn check_fake_ip(urls: &[RelayUrl]) -> Option<(String, Ipv4Addr)> {
    let mut seen = HashSet::new();
    for u in urls {
        let Some(host) = u.host_str() else { continue };
        let host = host.trim_end_matches('.');
        if host.parse::<IpAddr>().is_ok() || !seen.insert(host.to_ascii_lowercase()) {
            continue;
        }
        let port = u.port().unwrap_or(443);
        if let Ok(Ok(addrs)) =
            tokio::time::timeout(Duration::from_secs(2), tokio::net::lookup_host((host, port))).await
        {
            for addr in addrs {
                if let IpAddr::V4(ip) = addr.ip()
                    && is_fake_ip(ip)
                {
                    return Some((host.to_string(), ip));
                }
            }
        }
        if seen.len() >= 2 {
            break;
        }
    }
    None
}

async fn probe_own_relay_https(urls: &[RelayUrl]) -> Result<u64, String> {
    let ca = rc_relay::tls::ca();
    let client = reqwest::Client::builder()
        .tls_backend_preconfigured(ca)
        .no_proxy()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(4))
        .build()
        .map_err(|e| format!("{e}"))?;
    let mut last_err = String::new();
    for u in urls {
        let mut target = u.to_string();
        if let Some((base, q)) = target.split_once('?') {
            target = format!("{}/ping?{q}", base.trim_end_matches('/'));
        } else {
            target = format!("{}/ping", target.trim_end_matches('/'));
        }
        let t0 = Instant::now();
        match client.get(&target).send().await {
            Ok(resp) if resp.status().is_success() => {
                return Ok(t0.elapsed().as_millis().max(1) as u64);
            }
            Ok(resp) => {
                last_err = format!("HTTP {}", resp.status().as_u16());
            }
            Err(e) => {
                last_err = classify_reqwest_error(&e);
            }
        }
    }
    Err(last_err)
}

fn classify_reqwest_error(e: &reqwest::Error) -> String {
    let s = format!("{e:#}").to_ascii_lowercase();
    if e.is_timeout() || s.contains("timed out") {
        tr("连接超时（检查服务器 7843/TCP 防火墙或网络）", "Timed out (check server 7843/TCP firewall or network)").into()
    } else if s.contains("dns") || s.contains("failed to lookup") || s.contains("no such host") {
        tr("域名无法解析（检查 DNS 或域名记录）", "DNS lookup failed (check DNS or domain record)").into()
    } else if s.contains("certificate") || s.contains("tls") || s.contains("handshake") {
        tr("TLS 证书校验失败（中转码可能已过期，建议重新复制粘贴）", "TLS certificate mismatch (relay code may be outdated)").into()
    } else if s.contains("refused") || s.contains("10061") {
        tr("端口拒绝连接（检查 rc-relay 容器是否在运行及 7843/TCP 端口）", "Connection refused (check rc-relay container & port 7843/TCP)").into()
    } else {
        format!("{e}")
    }
}

// ---------------------------------------------------------------------------
// Local Windows inspection
// ---------------------------------------------------------------------------

fn inspect_local_env() -> LocalEnv {
    let egress_v4 = UdpSocket::bind("0.0.0.0:0")
        .ok()
        .and_then(|s| s.connect("1.1.1.1:53").ok().and_then(|()| s.local_addr().ok()))
        .and_then(|a| match a.ip() {
            IpAddr::V4(v4) if !v4.is_unspecified() => Some(v4),
            _ => None,
        });

    let mut egress_adapter: Option<AdapterSummary> = None;
    let mut physical_candidates: Vec<AdapterSummary> = Vec::new();
    let mut all_local_v4: Vec<Ipv4Addr> = Vec::new();
    let mut all_global_v6: Vec<Ipv6Addr> = Vec::new();
    let mut virtual_lans: Vec<String> = Vec::new();

    for iface in netdev::get_interfaces() {
        if !iface.is_up() || iface.is_loopback() {
            continue;
        }
        let friendly = iface
            .friendly_name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| iface.name.clone());
        let desc = iface.description.clone().unwrap_or_default();
        let combined = format!("{} {} {}", iface.name, friendly, desc).to_ascii_lowercase();

        let is_vm = is_vm_adapter(&combined);
        let is_vlan = is_virtual_lan_adapter(&combined);
        let ipv4s: Vec<Ipv4Addr> = iface
            .ipv4
            .iter()
            .map(|n| n.addr())
            .filter(|ip| !ip.is_loopback() && !ip.is_link_local())
            .collect();
        let has_link_local_v6 = iface.ipv6.iter().any(|n| is_link_local_v6(n.addr()));
        let global_v6s: Vec<Ipv6Addr> = iface
            .ipv6
            .iter()
            .zip(
                iface
                    .ipv6_addr_flags
                    .iter()
                    .map(Some)
                    .chain(std::iter::repeat(None)),
            )
            .filter(|(n, f)| {
                is_public_v6(n.addr()) && f.is_none_or(|fl| !fl.deprecated && !fl.tentative)
            })
            .map(|(n, _)| n.addr())
            .collect();
        let gateway_v4 = iface.gateway.as_ref().and_then(|g| g.ipv4.first().copied());
        let is_tun = !is_vlan
            && (is_tun_adapter(&combined, iface.if_type)
                || ipv4s.iter().any(|ip| is_fake_ip(*ip)));

        if is_vlan && !ipv4s.is_empty() {
            let tag = virtual_lan_brand(&combined).unwrap_or_else(|| friendly.clone());
            if !virtual_lans.contains(&tag) {
                virtual_lans.push(tag);
            }
        }
        if !is_vm {
            all_local_v4.extend(ipv4s.iter().copied());
            all_global_v6.extend(global_v6s.iter().copied());
        }

        let summary = AdapterSummary {
            friendly_name: friendly,
            description: desc,
            ipv4s,
            has_link_local_v6,
            global_v6s,
            gateway_v4,
            is_tun,
            is_vlan,
        };

        if let Some(ev4) = egress_v4
            && summary.ipv4s.contains(&ev4)
        {
            egress_adapter = Some(summary.clone());
        }

        if !is_vm && !is_vlan && !summary.is_tun && !summary.ipv4s.is_empty() {
            let is_phys_type = matches!(
                iface.if_type,
                InterfaceType::Ethernet | InterfaceType::Wireless80211 | InterfaceType::GigabitEthernet
            );
            if is_phys_type || summary.gateway_v4.is_some() {
                physical_candidates.push(summary);
            }
        }
    }

    // Prefer the physical adapter that matches egress_v4, else the one with an IPv4 gateway.
    let primary_physical = egress_adapter
        .as_ref()
        .filter(|a| !a.is_tun && !a.is_vlan)
        .cloned()
        .or_else(|| {
            physical_candidates
                .iter()
                .find(|a| a.gateway_v4.is_some())
                .cloned()
        })
        .or_else(|| physical_candidates.into_iter().next());

    let tun_adapter_name = egress_adapter
        .as_ref()
        .filter(|a| a.is_tun)
        .map(|a| {
            if !a.description.is_empty()
                && a.description.to_ascii_lowercase() != a.friendly_name.to_ascii_lowercase()
            {
                format!("{} ({})", a.friendly_name, a.description)
            } else {
                a.friendly_name.clone()
            }
        });

    LocalEnv {
        egress_v4,
        egress_adapter,
        primary_physical,
        all_local_v4,
        all_global_v6,
        virtual_lans,
        tun_adapter_name,
        ipv6_reg_disabled: check_ipv6_registry_disabled(),
        ipv6_detour: net::ipv6_detour(),
        system_proxy: read_system_proxy(),
        firewall: check_firewall_state(),
        net_profile: check_network_profile_state(),
    }
}

fn is_fake_ip(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 198 && (o[1] & 0xfe) == 18 // 198.18.0.0/15
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    ip.is_private() || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xe000) == 0x2000
}

fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

fn is_vm_adapter(label_lower: &str) -> bool {
    ["vethernet", "wsl", "hyper-v", "vmware", "virtualbox", "docker", "loopback"]
        .iter()
        .any(|k| label_lower.contains(k))
}

fn is_virtual_lan_adapter(label_lower: &str) -> bool {
    virtual_lan_brand(label_lower).is_some()
}

fn virtual_lan_brand(label_lower: &str) -> Option<String> {
    const BRANDS: [(&str, &str); 7] = [
        ("tailscale", "Tailscale"),
        ("easytier", "EasyTier"),
        ("zerotier", "ZeroTier"),
        ("radmin", "Radmin VPN"),
        ("hamachi", "Hamachi"),
        ("nebula", "Nebula"),
        ("headscale", "Headscale"),
    ];
    BRANDS
        .iter()
        .find(|(k, _)| label_lower.contains(k))
        .map(|(_, name)| (*name).to_string())
}

fn is_tun_adapter(label_lower: &str, if_type: InterfaceType) -> bool {
    const SUBSTRS: [&str; 18] = [
        "wintun",
        "clash",
        "mihomo",
        "sing-box",
        "singbox",
        "v2ray",
        "xray",
        "nekoray",
        "flclash",
        "clash verge",
        "shadowsocks",
        "wireguard",
        "openvpn",
        "tap-windows",
        "nordlynx",
        "mullvad",
        "cloudflare warp",
        "letsvpn",
    ];
    if SUBSTRS.iter().any(|k| label_lower.contains(k)) {
        return true;
    }
    let has_token = label_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| matches!(tok, "tun" | "utun" | "meta" | "warp" | "mihomo" | "clash"));
    if has_token {
        return true;
    }
    matches!(if_type, InterfaceType::Tunnel | InterfaceType::ProprietaryVirtual)
}

fn check_ipv6_registry_disabled() -> bool {
    let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SYSTEM\CurrentControlSet\Services\Tcpip6\Parameters")
    else {
        return false;
    };
    let Ok(val): Result<u32, _> = key.get_value("DisabledComponents") else {
        return false;
    };
    // Bit 4 (0x10) disables IPv6 on all non-tunnel interfaces; 0xff disables all IPv6.
    (val & 0x10) != 0 || (val & 0xff) == 0xff
}

fn read_system_proxy() -> Option<String> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    let enabled: u32 = key.get_value("ProxyEnable").unwrap_or(0);
    if enabled == 0 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").unwrap_or_default();
    let trimmed = server.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn normalize_exe_path(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"');
    let stripped = trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed);
    stripped.to_ascii_lowercase()
}

fn current_exe_normalized() -> Option<String> {
    let p = install::live_exe()
        .ok()
        .or_else(|| std::env::current_exe().ok())?;
    Some(normalize_exe_path(&p.to_string_lossy()))
}

struct ComGuard(bool);
impl ComGuard {
    fn init() -> Self {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self(hr.is_ok())
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}

unsafe fn variant_to_fw_rule(
    item: &windows::Win32::System::Variant::VARIANT,
) -> Option<windows::Win32::NetworkManagement::WindowsFirewall::INetFwRule> {
    use windows::{
        Win32::System::Variant::{VT_DISPATCH, VT_UNKNOWN},
        core::Interface,
    };
    let inner = unsafe { &item.Anonymous.Anonymous };
    if inner.vt == VT_DISPATCH {
        unsafe { inner.Anonymous.pdispVal.as_ref()?.cast().ok() }
    } else if inner.vt == VT_UNKNOWN {
        unsafe { inner.Anonymous.punkVal.as_ref()?.cast().ok() }
    } else {
        None
    }
}

fn check_firewall_state() -> FirewallState {
    use windows::{
        Win32::{
            NetworkManagement::WindowsFirewall::{
                INetFwPolicy2, NET_FW_ACTION_ALLOW, NET_FW_ACTION_BLOCK,
                NET_FW_IP_PROTOCOL_ANY, NET_FW_IP_PROTOCOL_UDP, NET_FW_PROFILE_TYPE2,
                NET_FW_PROFILE2_DOMAIN, NET_FW_PROFILE2_PRIVATE, NET_FW_PROFILE2_PUBLIC,
                NET_FW_RULE_DIR_IN, NetFwPolicy2,
            },
            System::{
                Com::{CLSCTX_ALL, CoCreateInstance},
                Ole::IEnumVARIANT,
                Variant::VARIANT,
            },
        },
        core::Interface,
    };

    let Some(my_exe) = current_exe_normalized() else {
        return FirewallState::Unknown;
    };
    let _com = ComGuard::init();
    unsafe {
        let Ok(policy): Result<INetFwPolicy2, _> =
            CoCreateInstance(&NetFwPolicy2, None, CLSCTX_ALL)
        else {
            return FirewallState::Unknown;
        };
        let Ok(active_profiles) = policy.CurrentProfileTypes() else {
            return FirewallState::Unknown;
        };
        let mut any_enabled = false;
        for prof in [
            NET_FW_PROFILE2_DOMAIN,
            NET_FW_PROFILE2_PRIVATE,
            NET_FW_PROFILE2_PUBLIC,
        ] {
            if (active_profiles & prof.0) != 0
                && policy
                    .get_FirewallEnabled(NET_FW_PROFILE_TYPE2(prof.0))
                    .is_ok_and(|v| v.as_bool())
            {
                any_enabled = true;
            }
        }
        if !any_enabled {
            return FirewallState::Disabled;
        }

        let Ok(rules) = policy.Rules() else {
            return FirewallState::Unknown;
        };
        let Ok(enum_unk) = rules._NewEnum() else {
            return FirewallState::Unknown;
        };
        let Ok(enum_var): Result<IEnumVARIANT, _> = enum_unk.cast() else {
            return FirewallState::Unknown;
        };

        let mut allowed = false;
        let mut blocked = false;
        let mut batch: [VARIANT; 32] = std::array::from_fn(|_| VARIANT::default());
        loop {
            let mut fetched = 0u32;
            let hr = enum_var.Next(&mut batch, &mut fetched);
            if fetched == 0 {
                break;
            }
            for item in batch.iter_mut().take(fetched as usize) {
                if let Some(rule) = variant_to_fw_rule(&*item)
                    && rule.Enabled().is_ok_and(|v| v.as_bool())
                    && rule.Direction().is_ok_and(|d| d == NET_FW_RULE_DIR_IN)
                    && rule.Profiles().is_ok_and(|p| (p & active_profiles) != 0)
                    && let Ok(app) = rule.ApplicationName()
                {
                    let app_str = app.to_string();
                    if !app_str.is_empty() && normalize_exe_path(&app_str) == my_exe {
                        let proto = rule.Protocol().unwrap_or(NET_FW_IP_PROTOCOL_ANY.0);
                        if proto == NET_FW_IP_PROTOCOL_ANY.0 || proto == NET_FW_IP_PROTOCOL_UDP.0 {
                            match rule.Action() {
                                Ok(a) if a == NET_FW_ACTION_BLOCK => blocked = true,
                                Ok(a) if a == NET_FW_ACTION_ALLOW => allowed = true,
                                _ => {}
                            }
                        }
                    }
                }
                *item = VARIANT::default();
            }
            if hr.is_err() || fetched < batch.len() as u32 {
                break;
            }
        }

        if blocked {
            FirewallState::Blocked
        } else if allowed {
            FirewallState::Allowed
        } else {
            FirewallState::MissingAllow
        }
    }
}

fn check_network_profile_state() -> NetProfileState {
    use windows::Win32::{
        Networking::NetworkListManager::{
            INetworkListManager, NLM_ENUM_NETWORK_CONNECTED, NLM_NETWORK_CATEGORY_PUBLIC,
            NetworkListManager,
        },
        System::Com::{CLSCTX_ALL, CoCreateInstance},
    };
    let _com = ComGuard::init();
    unsafe {
        let Ok(mgr): Result<INetworkListManager, _> =
            CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL)
        else {
            return NetProfileState::Unknown;
        };
        let Ok(nets) = mgr.GetNetworks(NLM_ENUM_NETWORK_CONNECTED) else {
            return NetProfileState::Unknown;
        };
        let mut saw_private_or_domain = false;
        let mut saw_public = false;
        let mut saw_public_internet = false;
        loop {
            let mut arr = [None];
            let mut fetched = 0u32;
            let hr = nets.Next(&mut arr, Some(&mut fetched));
            if fetched == 0 {
                break;
            }
            if let Some(net) = arr[0].take()
                && let Ok(cat) = net.GetCategory()
            {
                let name = net
                    .GetName()
                    .map(|b| b.to_string().to_ascii_lowercase())
                    .unwrap_or_default();
                let is_unidentified_virtual = name.contains("unidentified")
                    || name.contains("未识别")
                    || is_vm_adapter(&name)
                    || is_virtual_lan_adapter(&name);
                let internet = net.IsConnectedToInternet().is_ok_and(|v| v.as_bool());
                if cat == NLM_NETWORK_CATEGORY_PUBLIC {
                    if !is_unidentified_virtual {
                        saw_public = true;
                        if internet {
                            saw_public_internet = true;
                        }
                    }
                } else {
                    saw_private_or_domain = true;
                }
            }
            if hr.is_err() {
                break;
            }
        }
        if saw_public_internet || (saw_public && !saw_private_or_domain) {
            NetProfileState::Public
        } else if saw_private_or_domain {
            NetProfileState::PrivateOrDomain
        } else {
            NetProfileState::Unknown
        }
    }
}

// ---------------------------------------------------------------------------
// Elevated system fixes (invoked via `remote-control fix-net ...`)
// ---------------------------------------------------------------------------

fn fix_allow_firewall() -> Result<String> {
    let exe = install::live_exe().or_else(|_| std::env::current_exe().map_err(anyhow::Error::from))?;
    let my_exe = normalize_exe_path(&exe.to_string_lossy());

    // First remove any inbound BLOCK rule for this exe (Block always overrides Allow).
    let _ = remove_blocking_firewall_rules(&my_exe);

    let _ = Command::new("netsh")
        .creation_flags(CREATE_NO_WINDOW)
        .args([
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={FIREWALL_ALLOW_RULE}"),
        ])
        .output();

    let out = Command::new("netsh")
        .creation_flags(CREATE_NO_WINDOW)
        .args([
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={FIREWALL_ALLOW_RULE}"),
            "dir=in",
            "action=allow",
            &format!("program={}", exe.display()),
            "enable=yes",
            "profile=any",
        ])
        .output()
        .context("run netsh")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(anyhow!(
            "netsh failed ({}): {} {}",
            out.status,
            stdout.trim(),
            stderr.trim()
        ));
    }
    Ok(tr("已添加 Windows 防火墙放行规则", "Added Windows Firewall allow rule").into())
}

fn remove_blocking_firewall_rules(my_exe_norm: &str) -> Result<()> {
    use windows::{
        Win32::{
            NetworkManagement::WindowsFirewall::{
                INetFwPolicy2, NET_FW_ACTION_BLOCK, NET_FW_RULE_DIR_IN, NetFwPolicy2,
            },
            System::{
                Com::{CLSCTX_ALL, CoCreateInstance},
                Ole::IEnumVARIANT,
                Variant::VARIANT,
            },
        },
        core::Interface,
    };
    let _com = ComGuard::init();
    unsafe {
        let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_ALL)?;
        let rules = policy.Rules()?;
        let enum_var: IEnumVARIANT = rules._NewEnum()?.cast()?;
        let mut to_remove = Vec::new();
        let mut batch: [VARIANT; 32] = std::array::from_fn(|_| VARIANT::default());
        loop {
            let mut fetched = 0u32;
            let hr = enum_var.Next(&mut batch, &mut fetched);
            if fetched == 0 {
                break;
            }
            for item in batch.iter_mut().take(fetched as usize) {
                if let Some(rule) = variant_to_fw_rule(&*item)
                    && rule.Enabled().is_ok_and(|v| v.as_bool())
                    && rule.Direction().is_ok_and(|d| d == NET_FW_RULE_DIR_IN)
                    && rule.Action().is_ok_and(|a| a == NET_FW_ACTION_BLOCK)
                    && let Ok(app) = rule.ApplicationName()
                    && normalize_exe_path(&app.to_string()) == my_exe_norm
                    && let Ok(name) = rule.Name()
                {
                    to_remove.push(name);
                }
                *item = VARIANT::default();
            }
            if hr.is_err() || fetched < batch.len() as u32 {
                break;
            }
        }
        for name in to_remove {
            let _ = rules.Remove(&name);
        }
    }
    Ok(())
}

fn fix_enable_ipv6(adapter: Option<&str>, fix_registry: bool) -> Result<String> {
    if fix_registry {
        let (key, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
            .create_subkey(r"SYSTEM\CurrentControlSet\Services\Tcpip6\Parameters")
            .context("open Tcpip6 registry key")?;
        key.set_value("DisabledComponents", &0u32)
            .context("clear DisabledComponents")?;
    }
    if let Some(name) = adapter.filter(|s| !s.trim().is_empty()) {
        let escaped = name.replace('\'', "''");
        let ps_cmd = format!(
            "$ErrorActionPreference='Stop'; Enable-NetAdapterBinding -Name '{escaped}' -ComponentID ms_tcpip6"
        );
        let out = Command::new("powershell.exe")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", &ps_cmd])
            .output()
            .context("run powershell Enable-NetAdapterBinding")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(anyhow!(
                "{}",
                if err.is_empty() {
                    tr("开启网卡 IPv6 失败", "Failed to enable IPv6 on adapter").to_string()
                } else {
                    err
                }
            ));
        }
        // Give Windows a moment to assign fe80:: / SLAAC IPv6 before re-testing.
        std::thread::sleep(Duration::from_millis(1500));
    }
    if fix_registry {
        Ok(tr(
            "已恢复系统 IPv6 设置（若仍未生效，重启电脑即可）",
            "Re-enabled IPv6 (a reboot may be required if registry was disabled)",
        )
        .into())
    } else {
        Ok(tr("已开启网卡 IPv6 协议", "Enabled IPv6 on network adapter").into())
    }
}

fn fix_set_private_network() -> Result<String> {
    use windows::Win32::{
        Networking::NetworkListManager::{
            INetworkListManager, NLM_ENUM_NETWORK_CONNECTED, NLM_NETWORK_CATEGORY_PRIVATE,
            NLM_NETWORK_CATEGORY_PUBLIC, NetworkListManager,
        },
        System::Com::{CLSCTX_ALL, CoCreateInstance},
    };
    let _com = ComGuard::init();
    let mut changed = 0usize;
    unsafe {
        let mgr: INetworkListManager = CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL)
            .context("open NetworkListManager")?;
        let nets = mgr
            .GetNetworks(NLM_ENUM_NETWORK_CONNECTED)
            .context("enumerate connected networks")?;
        loop {
            let mut arr = [None];
            let mut fetched = 0u32;
            let hr = nets.Next(&mut arr, Some(&mut fetched));
            if fetched == 0 {
                break;
            }
            if let Some(net) = arr[0].take()
                && net.GetCategory().is_ok_and(|c| c == NLM_NETWORK_CATEGORY_PUBLIC)
            {
                let name = net
                    .GetName()
                    .map(|b| b.to_string().to_ascii_lowercase())
                    .unwrap_or_default();
                let is_unidentified_virtual = name.contains("unidentified")
                    || name.contains("未识别")
                    || is_vm_adapter(&name)
                    || is_virtual_lan_adapter(&name);
                if !is_unidentified_virtual {
                    net.SetCategory(NLM_NETWORK_CATEGORY_PRIVATE)
                        .context("set network category to Private")?;
                    changed += 1;
                }
            }
            if hr.is_err() {
                break;
            }
        }
    }
    if changed == 0 {
        return Err(anyhow!(
            "{}",
            tr("未找到需要切换的公用网络", "No Public network found to switch")
        ));
    }
    Ok(tr("已切换为专用网络", "Switched network profile to Private").into())
}

// ---------------------------------------------------------------------------
// Report synthesis
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_report(
    input: &DoctorInput,
    local: &LocalEnv,
    fake_ip: Option<(String, Ipv4Addr)>,
    net_report: Option<&NetReport>,
    distinct_host_count: usize,
    own_probes: &[OwnRelayProbe],
    pub_https_best: Option<(String, u64)>,
    pub_qad_best: Option<u64>,
    elapsed_ms: u64,
) -> DoctorReport {
    let mut items: Vec<CheckItem> = Vec::new();
    let router_url = local
        .primary_physical
        .as_ref()
        .and_then(|a| a.gateway_v4)
        .filter(|ip| is_private_v4(*ip))
        .map(|ip| format!("http://{ip}"));
    let remote_configured = input.public_relays || !own_probes.is_empty();
    let has_working_own_relay = own_probes.iter().any(|p| p.https_ms.is_some());
    let has_working_pub_relay = pub_https_best.is_some() || pub_qad_best.is_some();
    let udp_v4 = net_report.is_some_and(|r| r.udp_v4);
    let udp_v6 = net_report.is_some_and(|r| r.udp_v6);
    let global_v4 = net_report.and_then(|r| r.global_v4);
    let global_v6 = net_report.and_then(|r| r.global_v6);
    let mapping_varies_v4 = net_report.and_then(|r| r.mapping_varies_by_dest_ipv4);
    let has_public_v4 = global_v4.is_some_and(|g| local.all_local_v4.contains(g.ip()));

    // 1. Captive portal (only shown if detected AND probes actually failed — avoids false
    //    positives when a self-hosted relay has no port-80 /generate_204 responder)
    if net_report.and_then(|r| r.captive_portal) == Some(true)
        && !udp_v4
        && !has_working_own_relay
        && !has_working_pub_relay
    {
        items.push(CheckItem {
            id: "portal",
            level: Level::Err,
            title: tr(
                "网络被上网认证页拦截（校园网 / 酒店 Wi-Fi）",
                "Captive portal detected (campus / hotel Wi-Fi)",
            )
            .into(),
            detail: tr("外网 HTTP 探测被重定向到了登录页", "Outbound HTTP probe was intercepted by a login portal").into(),
            guidance: tr(
                "请先在浏览器完成网络登录认证，再点击重新检测。",
                "Complete the Wi-Fi web login in your browser, then re-test.",
            )
            .into(),
            actions: vec![FixAction::OpenUrl {
                label_zh: "打开认证页面",
                label_en: "Open Login Page",
                url: CAPTIVE_PORTAL_URL.into(),
            }],
        });
    }

    // 2. Proxy / TUN / Virtual LAN
    if local.tun_adapter_name.is_some() || fake_ip.is_some() {
        let mut detail_parts = Vec::new();
        if let Some(name) = &local.tun_adapter_name {
            detail_parts.push(if is_en() {
                format!("Default route goes through virtual adapter: {name}")
            } else {
                format!("默认路由正经过虚拟网卡：{name}")
            });
        }
        if let Some((host, ip)) = &fake_ip {
            detail_parts.push(if is_en() {
                format!("Relay host {host} resolved to Fake-IP {ip}")
            } else {
                format!("中转域名 {host} 被解析为 Fake-IP {ip}")
            });
        }
        items.push(CheckItem {
            id: "proxy",
            level: Level::Warn,
            title: tr(
                "检测到代理 TUN 模式接管流量",
                "Proxy TUN mode is intercepting traffic",
            )
            .into(),
            detail: detail_parts.join(if is_en() { "; " } else { "；" }),
            guidance: tr(
                "本软件默认不走系统代理，但 TUN 虚拟网卡会劫持 UDP 打洞和中转流量，导致打洞失败或延迟暴增。请在代理软件中把 remote-control.exe 设为直连（Clash/Mihomo 可直接点下方复制规则；v2rayN 在「设置 → 路由设置」添加进程直连）。",
                "Remote Control ignores system HTTP proxies, but TUN mode hijacks UDP hole-punching and relay traffic. Exclude remote-control.exe in your proxy app (copy the Clash/Mihomo rule below, or add a process bypass rule in v2rayN).",
            )
            .into(),
            actions: vec![FixAction::CopyText {
                label_zh: "复制 Clash 直连规则",
                label_en: "Copy Clash Direct Rule",
                text: CLASH_DIRECT_RULE.into(),
            }],
        });
    } else if !local.virtual_lans.is_empty() {
        let names = local.virtual_lans.join(" / ");
        let extra = if local.system_proxy.is_some() {
            tr("；系统代理已自动忽略，不受影响", "; system proxy is automatically ignored")
        } else {
            ""
        };
        items.push(CheckItem {
            id: "proxy",
            level: Level::Ok,
            title: if is_en() {
                format!("No TUN proxy interference (virtual LAN: {names})")
            } else {
                format!("无代理干扰（检测到虚拟局域网：{names}）")
            },
            detail: if is_en() {
                format!("Devices on the same virtual LAN can connect directly{extra}")
            } else {
                format!("同组虚拟局域网设备可直接直连{extra}")
            },
            guidance: String::new(),
            actions: Vec::new(),
        });
    } else if local.system_proxy.is_some() {
        items.push(CheckItem {
            id: "proxy",
            level: Level::Ok,
            title: tr(
                "代理环境正常（已自动忽略系统代理）",
                "Proxy OK (system proxy is ignored)",
            )
            .into(),
            detail: tr(
                "检测到 Windows 系统代理已开启，本软件默认直连不走系统代理，无需关闭",
                "Windows system proxy is active; Remote Control bypasses it automatically",
            )
            .into(),
            guidance: String::new(),
            actions: Vec::new(),
        });
    } else {
        items.push(CheckItem {
            id: "proxy",
            level: Level::Ok,
            title: tr("无代理或 TUN 干扰", "No proxy or TUN interference").into(),
            detail: String::new(),
            guidance: String::new(),
            actions: Vec::new(),
        });
    }

    // 3. UDP & NAT hole-punching qualification
    if !remote_configured {
        items.push(CheckItem {
            id: "nat",
            level: Level::Err,
            title: tr(
                "无法跨网连接（未启用公网辅助且无自建中转）",
                "Cross-network unavailable (public assist off & no private relay)",
            )
            .into(),
            detail: tr(
                "当前设置下仅能连接同一局域网内的设备",
                "With current settings, only devices on the same LAN can connect",
            )
            .into(),
            guidance: tr(
                "跨网使用请开启「公网辅助连接」（仅交换地址不传画面），或配置自建中转。",
                "Enable Public Relay Assist (address exchange only) or add a Private Relay.",
            )
            .into(),
            actions: vec![FixAction::EnablePublicRelays, FixAction::JumpRelays],
        });
    } else if has_public_v4 {
        items.push(CheckItem {
            id: "nat",
            level: Level::Ok,
            title: tr(
                "拥有公网 IPv4（最佳直连条件）",
                "Public IPv4 detected (best P2P condition)",
            )
            .into(),
            detail: global_v4
                .map(|g| if is_en() { format!("Public address: {g}") } else { format!("公网地址：{g}") })
                .unwrap_or_default(),
            guidance: String::new(),
            actions: Vec::new(),
        });
    } else if !udp_v4 {
        let any_tcp = has_working_own_relay || has_working_pub_relay;
        if any_tcp {
            let mut actions = Vec::new();
            if !has_working_own_relay {
                actions.push(FixAction::JumpRelays);
            }
            items.push(CheckItem {
                id: "nat",
                level: if has_working_own_relay { Level::Warn } else { Level::Err },
                title: tr(
                    "UDP 出网被拦截，无法 P2P 打洞直连",
                    "Outbound UDP is blocked — P2P hole-punching unavailable",
                )
                .into(),
                detail: tr(
                    "HTTPS 节点可通，但所有 UDP 探测均无回包",
                    "HTTPS reachable, but all UDP probes timed out",
                )
                .into(),
                guidance: if has_working_own_relay {
                    tr(
                        "这是运营商或企业防火墙对 UDP 的硬限制，修改本机设置通常无效。你已配置可用的自建中转，软件会自动走 TCP 7843 端口中转连接。",
                        "Your network firewall blocks UDP — local PC tweaks cannot bypass it. Your private relay is reachable and will automatically relay over TCP 7843.",
                    )
                    .into()
                } else {
                    tr(
                        "这是运营商或企业防火墙对 UDP 的硬限制，任何打洞软件都无法在此网络直连。唯一稳定解法是部署自建中转（软件会自动走 TCP 7843 端口）。",
                        "Your network firewall blocks all UDP, so P2P cannot work here. Deploy a private relay (automatically uses TCP 7843).",
                    )
                    .into()
                },
                actions,
            });
        } else {
            items.push(CheckItem {
                id: "nat",
                level: Level::Err,
                title: tr(
                    "无法连通任何外网探测节点",
                    "Cannot reach any remote probe server",
                )
                .into(),
                detail: tr(
                    "所有节点的 TCP 和 UDP 探测均超时或失败",
                    "Both TCP and UDP probes failed for all configured relays",
                )
                .into(),
                guidance: tr(
                    "请检查电脑是否能正常上网、是否被代理规则拦截，或自建中转服务器是否在线。",
                    "Check internet access, proxy rules, or whether your private relay is online.",
                )
                .into(),
                actions: vec![FixAction::JumpRelays],
            });
        }
    } else {
        match mapping_varies_v4 {
            Some(false) => {
                items.push(CheckItem {
                    id: "nat",
                    level: Level::Ok,
                    title: tr(
                        "圆锥型 NAT（具备 P2P 打洞资格）",
                        "Cone NAT (eligible for P2P hole-punching)",
                    )
                    .into(),
                    detail: global_v4
                        .map(|g| if is_en() { format!("Mapped external address: {g}") } else { format!("外网映射：{g}") })
                        .unwrap_or_default(),
                    guidance: String::new(),
                    actions: Vec::new(),
                });
            }
            Some(true) => {
                let mut actions = Vec::new();
                if let Some(url) = &router_url {
                    actions.push(FixAction::OpenUrl {
                        label_zh: "打开路由器后台",
                        label_en: "Open Router Admin",
                        url: url.clone(),
                    });
                }
                if !has_working_own_relay {
                    actions.push(FixAction::JumpRelays);
                }
                let guidance = if udp_v6 {
                    tr(
                        "IPv4 为对称 NAT，遇到同样严格的网络时无法靠 IPv4 打洞。好消息是本机 IPv6 可直连：只要对方也有 IPv6 即可绕过此限制；若对方无 IPv6，需借助自建中转。",
                        "IPv4 is Symmetric NAT, which fails against another strict NAT. Good news: your IPv6 works, so you can connect directly if the peer also has IPv6; otherwise use a private relay.",
                    )
                } else {
                    tr(
                        "这是路由器或运营商 NAT 策略限制，本机软件改不了。家宽可尝试在路由器开 IPv6 或光猫改桥接；公司/校园网通常无法修改，建议另一端开 IPv6、组 Tailscale/EasyTier，或配置自建中转。",
                        "Controlled by your router/ISP NAT — local tweaks cannot change it. On home broadband, enable IPv6 or router bridge mode; on corporate/campus networks, use IPv6 on both ends, Tailscale/EasyTier, or a private relay.",
                    )
                };
                items.push(CheckItem {
                    id: "nat",
                    level: Level::Warn,
                    title: tr(
                        "对称型 NAT（IPv4 打洞受限）",
                        "Symmetric NAT (IPv4 P2P limited)",
                    )
                    .into(),
                    detail: global_v4
                        .map(|g| {
                            if is_en() {
                                format!("External port changes per destination (sample: {g})")
                            } else {
                                format!("访问不同服务器时外网端口会变化（示例：{g}）")
                            }
                        })
                        .unwrap_or_default(),
                    guidance: guidance.into(),
                    actions,
                });
            }
            None => {
                if distinct_host_count < 2 {
                    items.push(CheckItem {
                        id: "nat",
                        level: Level::Info,
                        title: tr(
                            "IPv4 UDP 正常，但跳过 NAT 类型判断",
                            "IPv4 UDP works (NAT type skipped)",
                        )
                        .into(),
                        detail: global_v4
                            .map(|g| if is_en() { format!("Mapped external address: {g}") } else { format!("外网映射：{g}") })
                            .unwrap_or_default(),
                        guidance: tr(
                            "判断圆锥/对称 NAT 需要对比至少 2 个不同服务器；当前只配了 1 个自建中转且已关闭「公网辅助连接」，本工具不会私自访问公网节点。如需测出 NAT 类型，可临时开启公网辅助或添加第 2 个中转后重测。",
                            "Detecting Cone vs Symmetric NAT requires comparing at least 2 distinct servers. With only 1 private relay and Public Assist turned off, no public server is contacted. Enable Public Assist temporarily or add a 2nd relay to test NAT type.",
                        )
                        .into(),
                        actions: vec![FixAction::EnablePublicRelays],
                    });
                } else {
                    items.push(CheckItem {
                        id: "nat",
                        level: Level::Warn,
                        title: tr(
                            "仅 1 个节点收到 UDP 回包，未能判断 NAT 类型",
                            "Only 1 server replied over UDP — NAT type undetermined",
                        )
                        .into(),
                        detail: global_v4
                            .map(|g| if is_en() { format!("Mapped external address: {g}") } else { format!("外网映射：{g}") })
                            .unwrap_or_default(),
                        guidance: tr(
                            "其余节点 UDP 超时或不可达，可能是跨境链路丢包、服务器未放行 7842/UDP，或代理分流干扰。",
                            "Other servers timed out over UDP (possible packet loss, port 7842/UDP blocked, or proxy interference).",
                        )
                        .into(),
                        actions: Vec::new(),
                    });
                }
            }
        }
    }

    // 4. IPv6 Direct Connectivity (4 distinct root causes)
    let phys_name = local
        .primary_physical
        .as_ref()
        .map(|a| a.friendly_name.clone());
    let phys_missing_v6_binding = local
        .primary_physical
        .as_ref()
        .is_some_and(|a| !a.has_link_local_v6);

    if local.ipv6_reg_disabled || phys_missing_v6_binding {
        let detail = match (local.ipv6_reg_disabled, &phys_name) {
            (true, Some(n)) if phys_missing_v6_binding => {
                if is_en() {
                    format!("Windows registry disables IPv6 and adapter \"{n}\" has TCP/IPv6 unchecked")
                } else {
                    format!("系统注册表禁用了 IPv6，且网卡「{n}」未勾选 Internet 协议版本 6 (TCP/IPv6)")
                }
            }
            (true, _) => tr(
                "Windows 注册表 Tcpip6\\Parameters\\DisabledComponents 禁用了 IPv6",
                "Windows registry Tcpip6\\Parameters\\DisabledComponents disables IPv6",
            )
            .into(),
            (false, Some(n)) => {
                if is_en() {
                    format!("Adapter \"{n}\" has no fe80:: link-local IPv6 — \"Internet Protocol Version 6 (TCP/IPv6)\" is unchecked")
                } else {
                    format!("网卡「{n}」未启用 Internet 协议版本 6 (TCP/IPv6)（未分配 fe80:: 本地地址）")
                }
            }
            (false, None) => String::new(),
        };
        items.push(CheckItem {
            id: "ipv6",
            level: Level::Warn,
            title: tr(
                "本机网卡未开启 IPv6 协议",
                "IPv6 protocol is disabled on this PC",
            )
            .into(),
            detail,
            guidance: tr(
                "很多家宽、校园网和手机热点本身都提供公网 IPv6；开启后两端有 IPv6 就能绕过 IPv4 NAT 直接连通。点下方一键开启即可（也可在「网络连接 → 网卡属性」中手动勾选）。",
                "Most home broadband, campus Wi-Fi, and mobile hotspots provide public IPv6. Enabling it lets peers bypass IPv4 NAT directly.",
            )
            .into(),
            actions: vec![
                FixAction::EnableIpv6 {
                    adapter_name: if phys_missing_v6_binding { phys_name.clone() } else { None },
                    fix_registry: local.ipv6_reg_disabled,
                },
                FixAction::OpenNetworkAdapters,
            ],
        });
    } else if let Some(src) = local.ipv6_detour {
        items.push(CheckItem {
            id: "ipv6",
            level: Level::Warn,
            title: tr(
                "IPv6 经旁路由绕行，已自动停用 IPv6 直连",
                "IPv6 routed through a side router — IPv6 direct disabled",
            )
            .into(),
            detail: if is_en() {
                format!("Outbound IPv6 leaves from non-public address {src}")
            } else {
                format!("出网 IPv6 源地址为 {src}（非本机公网地址）")
            },
            guidance: tr(
                "旁路由接管 IPv6 会导致入站和回包路径不一致、握手卡死，程序已自动回退到 IPv4。如需恢复 IPv6 直连，请在旁路由关闭 IPv6 代理/劫持，或将本机 IPv6 网关指回主路由。",
                "Side-router IPv6 interception breaks direct handshakes, so IPv6 was automatically disabled. Turn off IPv6 proxying on the side router or point the IPv6 gateway back to your main router.",
            )
            .into(),
            actions: Vec::new(),
        });
    } else if udp_v6 {
        items.push(CheckItem {
            id: "ipv6",
            level: Level::Ok,
            title: tr("IPv6 可直连", "IPv6 direct connectivity ready").into(),
            detail: global_v6
                .map(|g| if is_en() { format!("Public IPv6: {g}") } else { format!("公网 IPv6：{g}") })
                .unwrap_or_default(),
            guidance: String::new(),
            actions: Vec::new(),
        });
    } else if let Some(local_v6) = local.all_global_v6.first().copied() {
        if remote_configured {
            let mut actions = Vec::new();
            if matches!(local.firewall, FirewallState::MissingAllow | FirewallState::Blocked) {
                actions.push(FixAction::AllowFirewall);
            }
            actions.push(FixAction::OpenNetworkAdapters);
            if let Some(url) = &router_url {
                actions.push(FixAction::OpenUrl {
                    label_zh: "打开路由器后台",
                    label_en: "Open Router Admin",
                    url: url.clone(),
                });
            }
            items.push(CheckItem {
                id: "ipv6",
                level: Level::Warn,
                title: tr(
                    "本机有公网 IPv6，但 IPv6 UDP 不通",
                    "Public IPv6 present, but IPv6 UDP failed",
                )
                .into(),
                detail: if is_en() {
                    format!("Local global IPv6: {local_v6} — no UDP reply received over IPv6")
                } else {
                    format!("本机公网地址：{local_v6}，但外网 IPv6 UDP 探测未收到回包")
                },
                guidance: tr(
                    "常见原因：1) Windows 防火墙拦截；2) 光猫/路由器 IPv6 防火墙拦截；3) 部分网卡驱动的「IPv6 校验和卸载」异常（可在「网络连接 → 网卡属性 → 配置 → 高级」里排查）。若上游限制改不了，IPv4 圆锥 NAT 仍可正常直连。",
                    "Common causes: Windows Firewall, router/ONT IPv6 firewall, or NIC IPv6 checksum offload bug (check Adapter Properties → Configure → Advanced). If IPv4 Cone NAT works, direct P2P still works.",
                )
                .into(),
                actions,
            });
        } else {
            items.push(CheckItem {
                id: "ipv6",
                level: Level::Info,
                title: tr("本机已获取公网 IPv6（未做外网探测）", "Public IPv6 assigned (not probed externally)").into(),
                detail: format!("{local_v6}"),
                guidance: String::new(),
                actions: Vec::new(),
            });
        }
    } else {
        let mut actions = Vec::new();
        if let Some(url) = &router_url {
            actions.push(FixAction::OpenUrl {
                label_zh: "打开路由器后台",
                label_en: "Open Router Admin",
                url: url.clone(),
            });
        }
        items.push(CheckItem {
            id: "ipv6",
            level: Level::Info,
            title: tr(
                "上游网络未分配公网 IPv6",
                "No public IPv6 assigned by router/ISP",
            )
            .into(),
            detail: tr(
                "本机网卡 IPv6 协议已正常开启，但路由器或宽带未下发公网 IPv6 地址",
                "IPv6 is enabled on this PC, but the router/ISP did not assign a public IPv6 address",
            )
            .into(),
            guidance: tr(
                "电脑本身无需改动。家庭宽带可在路由器后台开启 IPv6（原生/DHCPv6）或光猫改桥接；公司内网通常未开 IPv6，属正常现象，IPv4 打洞或自建中转一样可用。",
                "Your PC is fine. On home Wi-Fi, enable IPv6 in your router; corporate networks rarely offer IPv6 — IPv4 P2P or a private relay works just fine.",
            )
            .into(),
            actions,
        });
    }

    // 5. Windows Firewall
    match local.firewall {
        FirewallState::Blocked => {
            items.push(CheckItem {
                id: "firewall",
                level: Level::Err,
                title: tr(
                    "Windows 防火墙正在阻止本程序入站",
                    "Windows Firewall is blocking this app",
                )
                .into(),
                detail: tr(
                    "存在针对当前 exe 的入站阻止规则（常见于首次弹窗误点取消）",
                    "An inbound Block rule exists for this executable",
                )
                .into(),
                guidance: tr(
                    "点击下方一键修复，自动清理阻止规则并添加放行规则。",
                    "Click below to remove the block rule and allow inbound connections.",
                )
                .into(),
                actions: vec![FixAction::AllowFirewall],
            });
        }
        FirewallState::MissingAllow => {
            items.push(CheckItem {
                id: "firewall",
                level: Level::Warn,
                title: tr(
                    "Windows 防火墙尚未放行当前程序",
                    "No Windows Firewall allow rule for this executable",
                )
                .into(),
                detail: tr(
                    "未装后台服务或移动 exe 路径后，防火墙可能静默拦截入站直连与局域网发现",
                    "Without an allow rule, Windows Firewall may silently drop inbound P2P and LAN discovery",
                )
                .into(),
                guidance: tr(
                    "点击下方一键添加防火墙放行规则（支持专用与公用网络）。",
                    "Click below to add a Windows Firewall inbound allow rule for this executable.",
                )
                .into(),
                actions: vec![FixAction::AllowFirewall],
            });
        }
        FirewallState::Allowed => {
            items.push(CheckItem {
                id: "firewall",
                level: Level::Ok,
                title: tr("Windows 防火墙已放行", "Windows Firewall allows this app").into(),
                detail: String::new(),
                guidance: String::new(),
                actions: Vec::new(),
            });
        }
        FirewallState::Disabled => {
            items.push(CheckItem {
                id: "firewall",
                level: Level::Ok,
                title: tr("Windows 防火墙未开启（无拦截）", "Windows Firewall is off").into(),
                detail: String::new(),
                guidance: String::new(),
                actions: Vec::new(),
            });
        }
        FirewallState::Unknown => {}
    }

    // 6. Private relays & public assist status
    for p in own_probes {
        let qad_ms = match (p.qad_v4_ms, p.qad_v6_ms) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match (p.https_ms, qad_ms) {
            (Some(h), Some(q)) => {
                items.push(CheckItem {
                    id: "relay-own",
                    level: Level::Ok,
                    title: if is_en() {
                        format!("Private relay reachable: {}", p.display_host)
                    } else {
                        format!("自建中转正常：{}", p.display_host)
                    },
                    detail: format!("TCP(HTTPS) {h} ms · UDP(7842) {q} ms"),
                    guidance: String::new(),
                    actions: Vec::new(),
                });
            }
            (Some(h), None) if p.is_lan_relay => {
                items.push(CheckItem {
                    id: "relay-own",
                    level: Level::Ok,
                    title: if is_en() {
                        format!("Private relay reachable (LAN): {}", p.display_host)
                    } else {
                        format!("自建中转正常（局域网）：{}", p.display_host)
                    },
                    detail: format!("TCP(HTTPS) {h} ms"),
                    guidance: String::new(),
                    actions: Vec::new(),
                });
            }
            (Some(h), None) if p.v6_only_skipped_udp => {
                items.push(CheckItem {
                    id: "relay-own",
                    level: Level::Ok,
                    title: if is_en() {
                        format!("Private relay reachable (IPv6 TCP): {}", p.display_host)
                    } else {
                        format!("自建中转 TCP 可通（纯 IPv6）：{}", p.display_host)
                    },
                    detail: if is_en() {
                        format!("HTTPS {h} ms · IPv6 UDP skipped due to side-router IPv6 detour")
                    } else {
                        format!("HTTPS {h} ms · 因本机 IPv6 经旁路由绕行，未测 IPv6 UDP")
                    },
                    guidance: String::new(),
                    actions: Vec::new(),
                });
            }
            (Some(h), None) => {
                items.push(CheckItem {
                    id: "relay-own",
                    level: Level::Warn,
                    title: if is_en() {
                        format!("Private relay TCP OK, but 7842/UDP failed: {}", p.display_host)
                    } else {
                        format!("自建中转 TCP 可通，但 7842/UDP 不通：{}", p.display_host)
                    },
                    detail: if is_en() {
                        format!("HTTPS {h} ms, UDP(7842) timed out")
                    } else {
                        format!("HTTPS 延迟 {h} ms，但 UDP 7842 无回包")
                    },
                    guidance: tr(
                        "当前只能走 TCP 中转，且无法借助它协助打洞。请在云服务器安全组和系统防火墙放行 7842/UDP（若服务端主动设置了 enable_quic_addr_discovery = false 则属正常，可忽略）。",
                        "TCP relay works, but UDP hole-punching assist via this relay does not. Open 7842/UDP in your server firewall & cloud security group (ignore if enable_quic_addr_discovery = false on purpose).",
                    )
                    .into(),
                    actions: Vec::new(),
                });
            }
            (None, _) => {
                let err = p
                    .https_err
                    .clone()
                    .unwrap_or_else(|| tr("连接超时或不可达", "Timed out or unreachable").into());
                items.push(CheckItem {
                    id: "relay-own",
                    level: Level::Err,
                    title: if is_en() {
                        format!("Private relay unreachable: {}", p.display_host)
                    } else {
                        format!("自建中转连接失败：{}", p.display_host)
                    },
                    detail: err,
                    guidance: tr(
                        "请核对服务器防火墙 7843/TCP 与 7842/UDP、容器运行状态，或重新复制最新中转码粘贴到「设置 → 自建中转」。",
                        "Verify ports 7843/TCP & 7842/UDP, container status, or re-paste a fresh relay code in Settings → Private Relays.",
                    )
                    .into(),
                    actions: vec![FixAction::JumpRelays],
                });
            }
        }
    }

    if input.public_relays {
        if let Some((host, ms)) = &pub_https_best {
            let udp_note = match pub_qad_best {
                Some(q) => format!(" · UDP {q} ms"),
                None => String::new(),
            };
            items.push(CheckItem {
                id: "relay-pub",
                level: Level::Ok,
                title: tr("公网辅助节点已连通", "Public assist relay reachable").into(),
                detail: if is_en() {
                    format!("Fastest: {host} ({ms} ms{udp_note})")
                } else {
                    format!("最快节点：{host}（{ms} ms{udp_note}）")
                },
                guidance: String::new(),
                actions: Vec::new(),
            });
        } else if pub_qad_best.is_some() {
            items.push(CheckItem {
                id: "relay-pub",
                level: Level::Ok,
                title: tr("公网辅助节点 UDP 可达", "Public assist relay reachable over UDP").into(),
                detail: format!("UDP {} ms", pub_qad_best.unwrap_or(0)),
                guidance: String::new(),
                actions: Vec::new(),
            });
        } else {
            items.push(CheckItem {
                id: "relay-pub",
                level: if has_working_own_relay { Level::Info } else { Level::Err },
                title: tr("公共辅助节点均不可达", "Public assist relays unreachable").into(),
                detail: tr(
                    "4 个海外公共辅助节点（n0）全部超时或连接失败",
                    "All 4 public n0 assist relays timed out or failed",
                )
                .into(),
                guidance: if has_working_own_relay {
                    tr(
                        "你已有可用的自建中转，不影响正常使用；可在设置里关闭「公网辅助连接」。",
                        "Your private relay works fine — you can safely turn off Public Relay Assist.",
                    )
                    .into()
                } else {
                    tr(
                        "当前网络访问海外辅助节点受阻。建议部署国内/近地自建中转（不依赖公共节点）。",
                        "Outbound access to public relays is blocked or unstable. Deploying a private relay is recommended.",
                    )
                    .into()
                },
                actions: if has_working_own_relay {
                    Vec::new()
                } else {
                    vec![FixAction::JumpRelays]
                },
            });
        }
    }

    // 7. LAN discovery & Windows network profile
    if !input.lan_visible {
        items.push(CheckItem {
            id: "lan",
            level: Level::Info,
            title: tr("局域网发现已关闭", "LAN Discovery is turned off").into(),
            detail: tr("同局域网设备不会自动出现在临时设备列表中", "Devices on the same LAN won't auto-discover this PC").into(),
            guidance: tr(
                "如需同局域网免连接码直连，点击下方一键开启。",
                "Enable it if you want automatic same-LAN discovery without codes.",
            )
            .into(),
            actions: vec![FixAction::EnableLanDiscovery],
        });
    } else if local.net_profile == NetProfileState::Public {
        items.push(CheckItem {
            id: "lan",
            level: Level::Warn,
            title: tr(
                "Windows 当前网络类型为「公用网络」",
                "Windows network profile is set to Public",
            )
            .into(),
            detail: tr(
                "公用网络模式下 Windows 会限制局域网发现和部分入站连接",
                "Public profile restricts LAN discovery and some inbound connections",
            )
            .into(),
            guidance: tr(
                "如果是自家或信任的办公网络，建议点下方一键切为「专用网络」；如果在咖啡厅/酒店等公共 Wi-Fi，请保持公用不要切换。",
                "Switch to Private on trusted home/office networks; keep Public on cafe/hotel Wi-Fi.",
            )
            .into(),
            actions: vec![FixAction::SetPrivateNetwork, FixAction::OpenNetworkSettings],
        });
    } else {
        items.push(CheckItem {
            id: "lan",
            level: Level::Ok,
            title: tr("局域网环境正常", "LAN environment OK").into(),
            detail: tr(
                "局域网发现已开启，网络配置文件为专用/域网络",
                "LAN discovery enabled; network profile is Private/Domain",
            )
            .into(),
            guidance: String::new(),
            actions: Vec::new(),
        });
    }

    // Sort: Err → Warn → Ok → Info (problems always on top!).
    items.sort_by_key(|it| it.level);

    let err_count = items.iter().filter(|i| i.level == Level::Err).count();
    let warn_count = items.iter().filter(|i| i.level == Level::Warn).count();
    let is_direct_fix = |a: &FixAction| {
        matches!(
            a,
            FixAction::EnablePublicRelays
                | FixAction::EnableLanDiscovery
                | FixAction::EnableIpv6 { .. }
                | FixAction::AllowFirewall
                | FixAction::SetPrivateNetwork
        )
    };
    let fixable_err_count = items
        .iter()
        .filter(|i| i.level == Level::Err && i.actions.iter().any(is_direct_fix))
        .count();
    let fixable_warn_count = items
        .iter()
        .filter(|i| i.level == Level::Warn && i.actions.iter().any(is_direct_fix))
        .count();

    let (level, headline) = if err_count > 0 {
        let h = if fixable_err_count > 0 {
            if is_en() {
                format!("Found {err_count} blocking issue(s) ({fixable_err_count} can be fixed in 1 click)")
            } else {
                format!("发现 {err_count} 项阻断性问题（其中 {fixable_err_count} 项可一键修复）")
            }
        } else if is_en() {
            format!("Found {err_count} blocking issue(s) — cross-network connection may fail")
        } else {
            format!("发现 {err_count} 项阻断性问题，跨网连接可能失败")
        };
        (Level::Err, h)
    } else if warn_count > 0 {
        let h = if fixable_warn_count > 0 {
            if is_en() {
                format!("Usable, with {warn_count} limit/warning(s) ({fixable_warn_count} can be fixed in 1 click)")
            } else {
                format!("发现 {warn_count} 项限制或可优化项（其中 {fixable_warn_count} 项可一键修复）")
            }
        } else if mapping_varies_v4 == Some(true) && !udp_v6 && !has_working_own_relay {
            tr(
                "打洞条件受限（对称 NAT 且无 IPv6），建议配置自建中转",
                "P2P limited (Symmetric NAT without IPv6) — private relay recommended",
            )
            .into()
        } else if is_en() {
            format!("Mostly good, with {warn_count} item(s) to note")
        } else {
            format!("网络基本可用，有 {warn_count} 项限制或优化建议")
        };
        (Level::Warn, h)
    } else {
        (
            Level::Ok,
            tr(
                "网络良好，具备 P2P 打洞直连资格",
                "Network looks great — ready for direct P2P",
            )
            .into(),
        )
    };

    let mut summary_parts = Vec::new();
    if has_public_v4 {
        summary_parts.push(tr("公网 IPv4", "Public IPv4").to_string());
    } else if mapping_varies_v4 == Some(false) {
        summary_parts.push(tr("圆锥 NAT", "Cone NAT").to_string());
    } else if mapping_varies_v4 == Some(true) {
        summary_parts.push(tr("对称 NAT", "Symmetric NAT").to_string());
    } else if udp_v4 {
        summary_parts.push(tr("IPv4 UDP 通", "IPv4 UDP OK").to_string());
    }
    if udp_v6 {
        summary_parts.push(tr("IPv6 可直连", "IPv6 Direct").to_string());
    }
    if let Some(best_own_ms) = own_probes.iter().filter_map(|p| p.https_ms).min() {
        summary_parts.push(format!("{} {best_own_ms} ms", tr("自建中转", "Relay")));
    } else if let Some((_, ms)) = pub_https_best {
        summary_parts.push(format!("{} {ms} ms", tr("公网辅助", "Public assist")));
    }
    let summary = summary_parts.join(" · ");

    let mut raw_lines = Vec::new();
    if let Some(ev4) = local.egress_v4 {
        let aname = local
            .egress_adapter
            .as_ref()
            .map(|a| a.friendly_name.as_str())
            .unwrap_or("?");
        raw_lines.push(format!("egress_v4={ev4} adapter={aname}"));
    }
    if let Some(p) = &local.primary_physical {
        raw_lines.push(format!(
            "physical={} gw={:?} fe80={} global_v6={}",
            p.friendly_name,
            p.gateway_v4,
            p.has_link_local_v6,
            p.global_v6s.len()
        ));
    }
    if let Some(sp) = &local.system_proxy {
        raw_lines.push(format!("system_proxy={sp} (ignored)"));
    }
    if let Some((h, ip)) = &fake_ip {
        raw_lines.push(format!("fake_ip={h}->{ip}"));
    }
    if let Some(rep) = net_report {
        raw_lines.push(format!(
            "iroh udp_v4={} udp_v6={} symmetric_v4={:?} global_v4={:?} global_v6={:?} portal={:?}",
            rep.udp_v4,
            rep.udp_v6,
            rep.mapping_varies_by_dest_ipv4,
            rep.global_v4,
            rep.global_v6,
            rep.captive_portal
        ));
    }
    raw_lines.push(format!(
        "firewall={:?} profile={:?} ipv6_reg_disabled={} ipv6_detour={:?}",
        local.firewall, local.net_profile, local.ipv6_reg_disabled, local.ipv6_detour
    ));

    DoctorReport {
        level,
        headline,
        summary,
        items,
        raw_lines,
        elapsed_ms,
    }
}

fn clean_relay_label(url: &RelayUrl) -> String {
    let host = url.host_str().unwrap_or(url.as_str()).trim_end_matches('.');
    match url.port() {
        Some(p) if p != 443 => format!("{host}:{p}"),
        _ => host.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_ip_detection() {
        assert!(is_fake_ip(Ipv4Addr::new(198, 18, 0, 1)));
        assert!(is_fake_ip(Ipv4Addr::new(198, 19, 255, 254)));
        assert!(!is_fake_ip(Ipv4Addr::new(198, 20, 0, 1)));
        assert!(!is_fake_ip(Ipv4Addr::new(192, 168, 1, 1)));
    }

    #[test]
    fn tun_vs_vlan_classification() {
        assert!(is_tun_adapter("meta wintun userspace tunnel", InterfaceType::ProprietaryVirtual));
        assert!(is_tun_adapter("clash-verge tun", InterfaceType::Unknown));
        assert!(!is_tun_adapter("intel(r) ethernet connection", InterfaceType::Ethernet));
        assert!(is_virtual_lan_adapter("tailscale tunnel"));
        assert!(is_virtual_lan_adapter("easytier-tun"));
        assert!(!is_virtual_lan_adapter("wintun userspace tunnel"));
    }

    #[test]
    fn plain_text_omits_query_secrets() {
        let u: RelayUrl = "https://relay.example.com:7843/?rc-cert=secretpin123".parse().unwrap();
        assert_eq!(clean_relay_label(&u), "relay.example.com:7843");
    }

    #[test]
    fn single_private_relay_without_public_explains_skipped_nat() {
        let local = LocalEnv {
            egress_v4: Some(Ipv4Addr::new(192, 168, 1, 10)),
            egress_adapter: None,
            primary_physical: Some(AdapterSummary {
                friendly_name: "Ethernet".into(),
                description: "Intel".into(),
                ipv4s: vec![Ipv4Addr::new(192, 168, 1, 10)],
                has_link_local_v6: true,
                global_v6s: vec![],
                gateway_v4: Some(Ipv4Addr::new(192, 168, 1, 1)),
                is_tun: false,
                is_vlan: false,
            }),
            all_local_v4: vec![Ipv4Addr::new(192, 168, 1, 10)],
            all_global_v6: vec![],
            virtual_lans: vec![],
            tun_adapter_name: None,
            ipv6_reg_disabled: false,
            ipv6_detour: None,
            system_proxy: None,
            firewall: FirewallState::Allowed,
            net_profile: NetProfileState::PrivateOrDomain,
        };
        let mut nr = NetReport::default();
        nr.udp_v4 = true;
        nr.global_v4 = Some("203.0.113.5:51234".parse().unwrap());
        let input = DoctorInput {
            public_relays: false,
            lan_visible: true,
            own_relays: vec![vec!["https://relay.example.com:7843/".parse().unwrap()]],
        };
        let own = vec![OwnRelayProbe {
            display_host: "relay.example.com:7843".into(),
            https_ms: Some(25),
            qad_v4_ms: Some(26),
            qad_v6_ms: None,
            https_err: None,
            is_lan_relay: false,
            v6_only_skipped_udp: false,
        }];
        let rep = build_report(&input, &local, None, Some(&nr), 1, &own, None, None, 100);
        let nat = rep.items.iter().find(|i| i.id == "nat").expect("nat item");
        assert_eq!(nat.level, Level::Info);
        assert!(nat.guidance.contains("2"));

        // LAN relay with TCP only (QAD disabled on purpose) should report Ok, not Warn.
        let lan_own = vec![OwnRelayProbe {
            display_host: "home-relay.example.com:7843".into(),
            https_ms: Some(12),
            qad_v4_ms: None,
            qad_v6_ms: None,
            https_err: None,
            is_lan_relay: true,
            v6_only_skipped_udp: false,
        }];
        let rep2 = build_report(&input, &local, None, Some(&nr), 1, &lan_own, None, None, 80);
        let ritem = rep2.items.iter().find(|i| i.id == "relay-own").expect("relay item");
        assert_eq!(ritem.level, Level::Ok);
    }

    #[test]
    fn disabled_ipv6_and_tun_and_blocked_firewall_produce_actionable_fixes() {
        let local = LocalEnv {
            egress_v4: Some(Ipv4Addr::new(198, 18, 0, 1)),
            egress_adapter: None,
            primary_physical: Some(AdapterSummary {
                friendly_name: "Ethernet".into(),
                description: "Intel".into(),
                ipv4s: vec![Ipv4Addr::new(192, 168, 1, 10)],
                has_link_local_v6: false,
                global_v6s: vec![],
                gateway_v4: Some(Ipv4Addr::new(192, 168, 1, 1)),
                is_tun: false,
                is_vlan: false,
            }),
            all_local_v4: vec![Ipv4Addr::new(192, 168, 1, 10)],
            all_global_v6: vec![],
            virtual_lans: vec![],
            tun_adapter_name: Some("Meta (Wintun Userspace Tunnel)".into()),
            ipv6_reg_disabled: false,
            ipv6_detour: None,
            system_proxy: None,
            firewall: FirewallState::Blocked,
            net_profile: NetProfileState::Public,
        };
        let input = DoctorInput {
            public_relays: true,
            lan_visible: true,
            own_relays: vec![],
        };
        let mut nr = NetReport::default();
        nr.udp_v4 = true;
        nr.mapping_varies_by_dest_ipv4 = Some(true);
        nr.global_v4 = Some("203.0.113.5:51234".parse().unwrap());
        let rep = build_report(
            &input,
            &local,
            Some(("aps1-1.relay.n0.iroh.link".into(), Ipv4Addr::new(198, 18, 0, 12))),
            Some(&nr),
            4,
            &[],
            Some(("aps1-1.relay.n0.iroh.link".into(), 65)),
            Some(70),
            200,
        );
        assert_eq!(rep.level, Level::Err);
        let proxy = rep.items.iter().find(|i| i.id == "proxy").unwrap();
        assert_eq!(proxy.level, Level::Warn);
        assert!(proxy.actions.iter().any(|a| matches!(a, FixAction::CopyText { text, .. } if text == CLASH_DIRECT_RULE)));

        let ipv6 = rep.items.iter().find(|i| i.id == "ipv6").unwrap();
        assert_eq!(ipv6.level, Level::Warn);
        assert!(ipv6.actions.iter().any(|a| matches!(a, FixAction::EnableIpv6 { adapter_name: Some(n), fix_registry: false } if n == "Ethernet")));

        let fw = rep.items.iter().find(|i| i.id == "firewall").unwrap();
        assert_eq!(fw.level, Level::Err);
        assert!(fw.actions.contains(&FixAction::AllowFirewall));

        let lan = rep.items.iter().find(|i| i.id == "lan").unwrap();
        assert_eq!(lan.level, Level::Warn);
        assert!(lan.actions.contains(&FixAction::SetPrivateNetwork));
    }
}
