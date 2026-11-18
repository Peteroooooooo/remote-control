//! `desktop-probe selftest`: the whole secure-desktop test in one run.
//! Installs the service, raises a UAC prompt and clicks "Yes", opens and closes
//! the Ctrl+Alt+Del screen, locks the screen, collects the evidence, uninstalls
//! and checks that nothing is left. The only manual step is unlocking at the end.

use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use windows::{
    Win32::UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        Shell::{IsUserAnAdmin, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL},
    },
    core::{HSTRING, PCWSTR, w},
};
use winreg::{RegKey, enums::HKEY_LOCAL_MACHINE};

use crate::{DATA_DIR, HELPER_PIPE, INSTALL_DIR, SERVICE_NAME, SVC_PIPE, pipe, usercmd::marker_path};

const POLICY_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";

struct Out {
    file: File,
    passed: u32,
    failed: u32,
}

impl Out {
    fn say(&mut self, s: &str) {
        println!("{s}");
        let _ = writeln!(self.file, "{s}");
    }

    fn step(&mut self, name: &str, r: Result<String>) -> bool {
        match r {
            Ok(d) => {
                self.passed += 1;
                self.say(&format!("[通过] {name}：{d}"));
                true
            }
            Err(e) => {
                self.failed += 1;
                self.say(&format!("[失败] {name}：{e:#}"));
                false
            }
        }
    }
}

fn policy(name: &str) -> Option<u32> {
    RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(POLICY_KEY).ok()?.get_value::<u32, _>(name).ok()
}

fn helper(req: &str) -> Result<String> {
    let r = pipe::call(HELPER_PIPE, req)?;
    match r.strip_prefix("ok") {
        Some(rest) => Ok(rest.trim().to_string()),
        None => bail!("{r}"),
    }
}

fn service(req: &str) -> Result<String> {
    let r = pipe::call(SVC_PIPE, req)?;
    match r.strip_prefix("ok:") {
        Some(rest) => Ok(rest.trim().to_string()),
        None => bail!("{r}"),
    }
}

/// Waits until the input desktop is `name`; returns how long that took.
fn wait_desk(name: &str, limit: Duration) -> Result<f64> {
    let t = Instant::now();
    let mut last = String::new();
    while t.elapsed() < limit {
        if let Ok(d) = helper("desk") {
            if d.eq_ignore_ascii_case(name) {
                return Ok(t.elapsed().as_secs_f64());
            }
            last = d;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    bail!("{} 秒内没有切到 {name} 桌面（当前是 {last}）", limit.as_secs())
}

fn sleep_s(s: f64) {
    std::thread::sleep(Duration::from_secs_f64(s));
}

fn uac_step() -> Result<String> {
    let _ = std::fs::remove_file(marker_path());
    service("uac-test")?;
    // The service opened a small window in the middle of the screen as the user.
    // Clicking it is what a person would do; it gives that program the foreground,
    // which Windows requires before it shows the UAC prompt up front.
    sleep_s(2.0);
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    helper(&format!("click {} {}", w / 2, h / 2)).context("点击测试窗口失败")?;
    let t = wait_desk("Winlogon", Duration::from_secs(15)).context("UAC 弹窗没有出现在安全桌面上")?;
    // Let the dialog finish drawing and one screenshot be saved.
    sleep_s(2.0);
    let clicked = match helper("click-uac") {
        Ok(c) => c,
        Err(e) => {
            let _ = helper("key {ESC}");
            return Err(e.context("没找到或点不到“是”按钮，已按 Esc 关闭弹窗"));
        }
    };
    let t2 = Instant::now();
    let marker = loop {
        if let Ok(s) = std::fs::read_to_string(marker_path()) {
            break s;
        }
        if t2.elapsed() > Duration::from_secs(10) {
            bail!("点了按钮，但 10 秒内提权进程没有运行。点击记录：{clicked}");
        }
        sleep_s(0.25);
    };
    if !marker.contains("elevated=1") {
        bail!("提权进程运行了，但不是管理员权限：{}", marker.trim());
    }
    let _ = wait_desk("Default", Duration::from_secs(5));
    Ok(format!("{t:.1} 秒后切到安全桌面；{clicked}；以管理员权限启动的进程已运行"))
}

fn sas_step() -> Result<String> {
    service("sas")?;
    let t = wait_desk("Winlogon", Duration::from_secs(8)).context("Ctrl+Alt+Del 界面没有出现")?;
    sleep_s(2.0);
    helper("key {ESC}")?;
    let back = wait_desk("Default", Duration::from_secs(8)).context("按 Esc 后没有回到桌面")?;
    Ok(format!("{t:.1} 秒后出现 Ctrl+Alt+Del 界面；模拟按 Esc 后 {back:.1} 秒回到桌面"))
}

fn lock_step(o: &mut Out) -> Result<String> {
    o.say("");
    o.say("下一步会锁屏。锁屏后程序会自动点一下屏幕中间，让密码 / PIN 输入框出现。");
    o.say("看到输入框后，请你自己输入密码或 PIN 解锁。最多等 3 分钟。");
    for i in (1..=5).rev() {
        println!("  {i} 秒后锁屏…");
        sleep_s(1.0);
    }
    service("lock")?;
    let t = wait_desk("Winlogon", Duration::from_secs(8)).context("锁屏界面没有出现")?;
    sleep_s(3.0);
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let click = helper(&format!("click {} {}", w / 2, h / 2));
    sleep_s(3.0);
    let back = wait_desk("Default", Duration::from_secs(180)).context("3 分钟内没有解锁")?;
    let click = match click {
        Ok(_) => format!("在锁屏上点了 ({}, {})", w / 2, h / 2),
        Err(e) => format!("在锁屏上点击失败：{e:#}"),
    };
    Ok(format!("{t:.1} 秒后出现锁屏；{click}；你在 {back:.0} 秒内解锁"))
}

/// Copies probe.log and the screenshots worth looking at into `out`.
fn collect(out: &Path) -> Result<String> {
    let data = PathBuf::from(DATA_DIR);
    std::fs::copy(data.join("probe.log"), out.join("probe.log")).context("copy probe.log")?;
    let mut shots: Vec<PathBuf> = std::fs::read_dir(data.join("shots"))
        .context("shots folder")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    shots.sort();
    let secure: Vec<&PathBuf> =
        shots.iter().filter(|p| p.to_string_lossy().to_lowercase().ends_with("_winlogon.png")).collect();
    let dir = out.join("shots");
    std::fs::create_dir_all(&dir)?;
    for p in secure.iter().copied().chain(shots.iter().rev().take(2)) {
        std::fs::copy(p, dir.join(p.file_name().expect("file"))).context("copy screenshot")?;
    }
    if secure.is_empty() {
        bail!("一共 {} 张截图，没有一张来自安全桌面", shots.len());
    }
    Ok(format!("{} 张截图里有 {} 张来自安全桌面，已复制到 shots\\", shots.len(), secure.len()))
}

fn cleanup_check(sas_before: Option<u32>) -> Result<String> {
    crate::install::uninstall()?;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    if mgr.open_service(SERVICE_NAME, windows_service::service::ServiceAccess::QUERY_STATUS).is_ok() {
        bail!("服务还在");
    }
    for d in [INSTALL_DIR, DATA_DIR] {
        if Path::new(d).exists() {
            bail!("{d} 还在");
        }
    }
    let after = policy("SoftwareSASGeneration");
    if after != sas_before {
        bail!("SoftwareSASGeneration 安装前是 {sas_before:?}，卸载后是 {after:?}");
    }
    Ok("服务、程序文件、数据文件夹都已删除，Ctrl+Alt+Del 策略已恢复原样".into())
}

/// Starts this exe again elevated with the same output folder; the user answers one UAC prompt.
fn relaunch_elevated(out: &Path) -> Result<()> {
    let exe = HSTRING::from(std::env::current_exe()?.as_os_str());
    let params = HSTRING::from(format!("selftest --out \"{}\"", out.display()));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.context("需要管理员权限：请在弹出的窗口里点“是”")?;
    println!("测试已在新的管理员窗口里运行，结果会写到 {} 下的 uac-<时间> 文件夹", out.display());
    Ok(())
}

/// `parent`: folder that receives one `uac-<time>` subfolder per run.
pub fn run(parent: Option<PathBuf>) -> Result<()> {
    let parent = match parent {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    if !unsafe { IsUserAnAdmin() }.as_bool() {
        return relaunch_elevated(&parent);
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let out = parent.join(format!("uac-{}", crate::timestamp()));
    std::fs::create_dir_all(&out)?;
    let mut o = Out { file: File::create(out.join("result.txt"))?, passed: 0, failed: 0 };
    o.say(&format!("desktop-probe 自动测试 {}", crate::timestamp()));
    o.say("测试期间请不要动鼠标和键盘，直到提示你解锁。");
    o.say(&format!(
        "UAC 设置：EnableLUA={:?} ConsentPromptBehaviorAdmin={:?} PromptOnSecureDesktop={:?}",
        policy("EnableLUA"),
        policy("ConsentPromptBehaviorAdmin"),
        policy("PromptOnSecureDesktop")
    ));
    let sas_before = policy("SoftwareSASGeneration");
    o.say("");

    let installed = o.step(
        "1 安装服务",
        crate::install::install().and_then(|()| {
            let t = wait_desk("Default", Duration::from_secs(20)).context("辅助进程没有启动")?;
            Ok(format!("服务在运行，辅助进程 {t:.1} 秒内就绪"))
        }),
    );
    if installed {
        o.step("2 看到并点击 UAC 弹窗", uac_step());
        o.step("3 Ctrl+Alt+Del 界面", sas_step());
        let r = lock_step(&mut o);
        o.step("4 锁屏", r);
        o.step("5 收集截图和日志", collect(&out));
    }
    o.step("6 卸载并检查没有残留", cleanup_check(sas_before));
    o.say("");
    o.say(&format!("通过 {} 项，失败 {} 项。结果在 {}", o.passed, o.failed, out.display()));
    o.say("可选：重启后登录界面的测试见 TESTING.md 最后一节。");
    println!("\n按回车关闭窗口。");
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    Ok(())
}
