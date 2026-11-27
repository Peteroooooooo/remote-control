//! Dynamic per-session phone virtual display manager for Windows 10/11.
//!
//! Auto-detects an installed user-mode IddCx virtual display driver:
//! 1. **ZakoVDD** (`Zako Display Adapter`, `GUID_DEVINTERFACE_ZAKO_VDD_CONTROL`)
//! 2. **SudoVDA** (`SudoMaker SudoVDA`, `SUVDA_INTERFACE_GUID`)
//!
//! Creates a virtual monitor matching the phone's exact portrait/landscape resolution,
//! applies per-monitor DPI scaling immediately via `DisplayConfigSetDeviceInfo` (no
//! sign-out, zero impact on physical monitors), and tears down cleanly on exit/drop.

use std::{
    collections::HashSet,
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Devices::{
            DeviceAndDriverInstallation::{
                DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, SP_DEVICE_INTERFACE_DATA,
                SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList,
                SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
            },
            Display::{
                DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_TYPE,
                DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
                DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
                DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
                QDC_VIRTUAL_MODE_AWARE, QueryDisplayConfig,
            },
        },
        Foundation::{CloseHandle, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE, LUID, POINTL, RECT},
        Graphics::Gdi::{
            CDS_NORESET, CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW,
            DEVMODEW_0, DEVMODEW_0_1, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT,
            DM_PELSWIDTH, DM_POSITION, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
        },
        Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::IO::DeviceIoControl,
    },
    core::{GUID, HSTRING, PCWSTR},
};

use crate::log;

/// ZakoVDD (`qiin2333/zako-vdd` / Foundation Sunshine) control interface GUID.
const GUID_ZAKO_VDD: GUID = GUID::from_values(
    0xDA9F_8C2B,
    0x7E4F,
    0x49A1,
    [0x9D, 0x4E, 0x6F, 0x2B, 0x0E, 0x1A, 0x0C, 0x4D],
);

/// SudoVDA (`SudoMaker/SudoVDA` / Apollo) control interface GUID.
const GUID_SUDOVDA: GUID = GUID::from_values(
    0xE5BC_C234,
    0x1E0C,
    0x418A,
    [0xA0, 0xD4, 0xEF, 0x8B, 0x75, 0x01, 0x41, 0x4D],
);

/// Stable monitor GUID for our phone virtual screen so Windows tracks its layout cleanly.
const PHONE_MONITOR_GUID: GUID = GUID::from_values(
    0x8B42_C1F0,
    0x73A9,
    0x4E5B,
    [0x9C, 0x12, 0x4A, 0x7E, 0x2D, 0x8B, 0x00, 0x01],
);
const PHONE_MONITOR_GUID_STR: &str = "{8B42C1F0-73A9-4E5B-9C12-4A7E2D8B0001}";

// CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, func, METHOD_BUFFERED=0, access)
const IOCTL_ZAKO_COMMAND: u32 = (0x22 << 16) | (2 << 14) | (0x800 << 2);
const IOCTL_ZAKO_PING: u32 = (0x22 << 16) | (1 << 14) | (0x801 << 2);

const IOCTL_SUDO_ADD: u32 = (0x22 << 16) | (0x800 << 2);
const IOCTL_SUDO_REMOVE: u32 = (0x22 << 16) | (0x801 << 2);
const IOCTL_SUDO_PING: u32 = (0x22 << 16) | (0x888 << 2);
const IOCTL_SUDO_VERSION: u32 = (0x22 << 16) | (0x8FF << 2);

// Undocumented DisplayConfig DPI packets used by Windows Settings.
const DISPLAYCONFIG_DEVICE_INFO_GET_DPI_SCALE: i32 = -3;
const DISPLAYCONFIG_DEVICE_INFO_SET_DPI_SCALE: i32 = -4;

const DPI_SCALE_STEPS: &[u16] = &[100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

#[repr(C)]
struct SudoAddParams {
    width: u32,
    height: u32,
    refresh_rate: u32,
    monitor_guid: GUID,
    device_name: [u8; 14],
    serial_number: [u8; 14],
}

#[repr(C)]
#[derive(Default)]
struct SudoAddOut {
    adapter_luid: LUID,
    target_id: u32,
}

#[repr(C)]
struct SudoRemoveParams {
    monitor_guid: GUID,
}

#[repr(C)]
#[derive(Default)]
struct SudoVersionOut {
    major: u8,
    minor: u8,
    incremental: u8,
    test_build: u8,
}

#[repr(C)]
struct DisplayConfigDpiGet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    min_scale_rel: i32,
    cur_scale_rel: i32,
    max_scale_rel: i32,
}

#[repr(C)]
struct DisplayConfigDpiSet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    scale_rel: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    ZakoVdd,
    SudoVda,
}

#[derive(Clone, Debug)]
pub struct ActiveVirtualDisplay {
    pub device_name: String,
    pub rect: RECT,
    pub width: u16,
    pub height: u16,
    pub dpi_percent: u16,
}

struct OwnedHandle(HANDLE);
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

pub struct VirtualDisplayManager {
    backend: Option<BackendKind>,
    active: Option<ActiveVirtualDisplay>,
    sudo_target: Option<(LUID, u32)>,
    ping_stop: Option<Arc<AtomicBool>>,
    ping_thread: Option<JoinHandle<()>>,
}

impl Default for VirtualDisplayManager {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualDisplayManager {
    pub fn new() -> Self {
        let backend = detect_backend();
        if let Some(b) = backend {
            log!("vdisplay: detected backend {b:?}");
        } else {
            log!("vdisplay: no supported virtual display driver detected (will use physical displays)");
        }
        Self {
            backend,
            active: None,
            sudo_target: None,
            ping_stop: None,
            ping_thread: None,
        }
    }

    pub fn is_supported(&self) -> bool {
        self.backend.is_some()
    }

    pub fn active(&self) -> Option<&ActiveVirtualDisplay> {
        self.active.as_ref()
    }

    /// Computes a crisp, readable Windows DPI scale percent for the target client viewport.
    pub fn recommended_dpi(w: u16, h: u16) -> u16 {
        let short = w.min(h);
        if w < h {
            // Phone portrait (e.g. 1080x2412 -> 225% => ~480x1072 logical)
            if short >= 1300 { 250 } else if short >= 900 { 225 } else { 175 }
        } else if short >= 1350 {
            // High-res tablet landscape (e.g. 2560x1600 -> 150%)
            150
        } else {
            // Phone landscape (e.g. 2412x1080 -> 175% => ~1378x617 logical)
            175
        }
    }

    /// Ensures the phone virtual display exists at `(width, height)` with `dpi_percent`
    /// (`0` = auto recommended DPI). Returns the active monitor metadata.
    pub fn ensure(&mut self, raw_w: u16, raw_h: u16, raw_dpi: u16) -> Result<ActiveVirtualDisplay> {
        let backend = self.backend.ok_or_else(|| anyhow::anyhow!("no virtual display driver available"))?;
        let (w, h) = normalize_dimensions(raw_w, raw_h);
        let dpi = snap_dpi(if raw_dpi == 0 { Self::recommended_dpi(w, h) } else { raw_dpi });

        // Fast path: already active at matching resolution; just update DPI if needed.
        if let Some(cur) = self.active.clone() {
            if cur.width == w && cur.height == h {
                if cur.dpi_percent != dpi {
                    if let Some(info) = find_path_by_device_name(&cur.device_name) {
                        let applied = set_monitor_dpi(info.adapter_luid, info.source_id, dpi);
                        log!("vdisplay: updated DPI on {} to {}% (ok={applied})", cur.device_name, dpi);
                    }
                    let mut updated = cur;
                    updated.dpi_percent = dpi;
                    self.active = Some(updated.clone());
                    return Ok(updated);
                }
                return Ok(cur);
            }
        }

        match backend {
            BackendKind::ZakoVdd => self.ensure_zako(w, h, dpi),
            BackendKind::SudoVda => self.ensure_sudo(w, h, dpi),
        }
    }

    /// Steps the active virtual display's DPI down by one notch if a window's minimum
    /// width exceeds the virtual display's logical width, giving the app room to breathe.
    #[allow(dead_code)]
    pub fn widen_for_min_width(&mut self, needed_px: i32) -> Option<ActiveVirtualDisplay> {
        let cur = self.active.clone()?;
        let cur_w = cur.rect.right - cur.rect.left;
        if needed_px <= cur_w + 8 || cur.dpi_percent <= 125 {
            return None;
        }
        let next_dpi = match cur.dpi_percent {
            250.. => 200,
            225 => 175,
            200 => 150,
            175 => 150,
            _ => 125,
        };
        if next_dpi == cur.dpi_percent {
            return None;
        }
        self.ensure(cur.width, cur.height, next_dpi).ok()
    }

    /// Destroys the virtual display and stops any watchdog keepalive thread.
    pub fn destroy(&mut self) {
        self.stop_ping();
        let Some(backend) = self.backend else {
            self.active = None;
            return;
        };
        if self.active.is_none() && self.sudo_target.is_none() {
            return;
        }
        let dev_name = self.active.as_ref().map(|a| a.device_name.clone());
        match backend {
            BackendKind::ZakoVdd => {
                if let Err(e) = zako_send_command("DESTROYMONITOR") {
                    log!("vdisplay: ZakoVDD DESTROYMONITOR failed: {e:#}");
                } else {
                    log!("vdisplay: ZakoVDD DESTROYMONITOR sent");
                    if let Some(name) = dev_name {
                        let deadline = Instant::now() + Duration::from_millis(1500);
                        while Instant::now() < deadline {
                            if find_path_by_device_name(&name).is_none() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(50));
                        }
                    }
                }
            }
            BackendKind::SudoVda => {
                if let Err(e) = sudo_remove_display(PHONE_MONITOR_GUID) {
                    log!("vdisplay: SudoVDA remove failed: {e:#}");
                } else {
                    log!("vdisplay: SudoVDA virtual display removed");
                }
                self.sudo_target = None;
            }
        }
        self.active = None;
    }

    fn ensure_zako(&mut self, w: u16, h: u16, dpi: u16) -> Result<ActiveVirtualDisplay> {
        let already_created = self.active.is_some();
        let existing_names: HashSet<String> = list_active_paths()
            .into_iter()
            .map(|p| p.device_name)
            .collect();

        // Register both target orientation and its rotated twin so rotation is instant.
        let modes_cmd = format!("SETMODES {w}x{h}x60,{h}x{w}x60");
        let _ = zako_send_command(&modes_cmd);

        if !already_created {
            // Check if a Zako display is already lingering from an earlier run before creating.
            if find_zako_path().is_none() {
                let create_cmd = format!("CREATEMONITOR {PHONE_MONITOR_GUID_STR}:[1000,0,1000]");
                zako_send_command(&create_cmd).context("ZakoVDD CREATEMONITOR")?;
            }
        }

        // Wait for the Zako virtual display to appear in active display paths.
        let path = wait_for_path(Duration::from_millis(2500), || {
            if let Some(cur) = self.active.as_ref()
                && let Some(p) = find_path_by_device_name(&cur.device_name)
            {
                return Some(p);
            }
            if let Some(p) = find_zako_path() {
                return Some(p);
            }
            list_active_paths()
                .into_iter()
                .find(|p| !existing_names.contains(&p.device_name))
        })
        .ok_or_else(|| anyhow::anyhow!("ZakoVDD monitor did not become active in time"))?;

        // Re-apply SETMODES after monitor creation in case the driver only applies modes to live targets.
        let _ = zako_send_command(&modes_cmd);
        thread::sleep(Duration::from_millis(80));

        // Position virtual display cleanly to the right of all other active displays (never hijack primary).
        let right_edge = rightmost_other_display_x(&path.device_name);
        apply_display_mode(&path.device_name, w as u32, h as u32, 60, right_edge, 0)?;
        thread::sleep(Duration::from_millis(120));

        // Re-query CCD paths so recommended DPI index reflects the new resolution.
        let settled = find_path_by_device_name(&path.device_name).unwrap_or(path);
        let dpi_ok = set_monitor_dpi(settled.adapter_luid, settled.source_id, dpi);
        if !dpi_ok {
            log!("vdisplay: DisplayConfigSetDeviceInfo DPI {dpi}% returned non-zero on {}", settled.device_name);
        }
        thread::sleep(Duration::from_millis(60));

        let rect = query_display_rect(&settled.device_name).unwrap_or(RECT {
            left: right_edge,
            top: 0,
            right: right_edge + w as i32,
            bottom: h as i32,
        });
        let actual_w = (rect.right - rect.left).max(2) as u16;
        let actual_h = (rect.bottom - rect.top).max(2) as u16;
        let info = ActiveVirtualDisplay {
            device_name: settled.device_name.clone(),
            rect,
            width: actual_w,
            height: actual_h,
            dpi_percent: dpi,
        };
        log!(
            "vdisplay: ZakoVDD active on {} {}x{} at ({},{}) @ {}% DPI",
            info.device_name,
            info.width,
            info.height,
            info.rect.left,
            info.rect.top,
            info.dpi_percent,
        );
        self.active = Some(info.clone());
        Ok(info)
    }

    fn ensure_sudo(&mut self, w: u16, h: u16, dpi: u16) -> Result<ActiveVirtualDisplay> {
        if self.active.is_some() {
            // Re-create cleanly with new native resolution so portrait/landscape swaps cleanly.
            let _ = sudo_remove_display(PHONE_MONITOR_GUID);
            self.active = None;
            self.sudo_target = None;
            thread::sleep(Duration::from_millis(100));
        }

        let out = sudo_add_display(w as u32, h as u32, 60, PHONE_MONITOR_GUID)?;
        self.sudo_target = Some((out.adapter_luid, out.target_id));
        self.start_sudo_ping();

        let path = wait_for_path(Duration::from_millis(2500), || {
            list_active_paths().into_iter().find(|p| {
                p.adapter_luid.LowPart == out.adapter_luid.LowPart
                    && p.adapter_luid.HighPart == out.adapter_luid.HighPart
                    && p.target_id == out.target_id
            })
        })
        .ok_or_else(|| anyhow::anyhow!("SudoVDA monitor did not appear in active display paths"))?;

        let right_edge = rightmost_other_display_x(&path.device_name);
        let _ = apply_display_mode(&path.device_name, w as u32, h as u32, 60, right_edge, 0);
        thread::sleep(Duration::from_millis(120));

        let settled = find_path_by_device_name(&path.device_name).unwrap_or(path);
        let _ = set_monitor_dpi(settled.adapter_luid, settled.source_id, dpi);
        thread::sleep(Duration::from_millis(60));

        let rect = query_display_rect(&settled.device_name).unwrap_or(RECT {
            left: right_edge,
            top: 0,
            right: right_edge + w as i32,
            bottom: h as i32,
        });
        let actual_w = (rect.right - rect.left).max(2) as u16;
        let actual_h = (rect.bottom - rect.top).max(2) as u16;
        let info = ActiveVirtualDisplay {
            device_name: settled.device_name.clone(),
            rect,
            width: actual_w,
            height: actual_h,
            dpi_percent: dpi,
        };
        log!(
            "vdisplay: SudoVDA active on {} {}x{} at ({},{}) @ {}% DPI",
            info.device_name,
            info.width,
            info.height,
            info.rect.left,
            info.rect.top,
            info.dpi_percent,
        );
        self.active = Some(info.clone());
        Ok(info)
    }

    fn start_sudo_ping(&mut self) {
        if self.ping_thread.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = thread::Builder::new()
            .name("vdisplay-ping".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    let _ = sudo_ping();
                    for _ in 0..10 {
                        if flag.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .ok();
        self.ping_stop = Some(stop);
        self.ping_thread = handle;
    }

    fn stop_ping(&mut self) {
        if let Some(stop) = self.ping_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some(h) = self.ping_thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for VirtualDisplayManager {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn normalize_dimensions(raw_w: u16, raw_h: u16) -> (u16, u16) {
    let mut w = (raw_w.clamp(480, 2560)) & !1;
    let mut h = (raw_h.clamp(480, 2560)) & !1;
    if w == 0 {
        w = 1080;
    }
    if h == 0 {
        h = 2400;
    }
    (w, h)
}

fn snap_dpi(target: u16) -> u16 {
    DPI_SCALE_STEPS
        .iter()
        .copied()
        .min_by_key(|&v| v.abs_diff(target))
        .unwrap_or(200)
        .clamp(100, 300)
}

fn detect_backend() -> Option<BackendKind> {
    if zako_ping() {
        return Some(BackendKind::ZakoVdd);
    }
    if sudo_check_version() {
        return Some(BackendKind::SudoVda);
    }
    None
}

fn open_device_interface(guid: &GUID) -> Result<OwnedHandle> {
    unsafe {
        let dev_info = SetupDiGetClassDevsW(
            Some(guid),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .context("SetupDiGetClassDevsW")?;

        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        let enum_res = SetupDiEnumDeviceInterfaces(dev_info, None, guid, 0, &mut iface);
        if enum_res.is_err() {
            let _ = SetupDiDestroyDeviceInfoList(dev_info);
            bail!("virtual display interface not present");
        }

        let mut required: u32 = 0;
        let _ = SetupDiGetDeviceInterfaceDetailW(dev_info, &iface, None, 0, Some(&mut required), None);
        if required == 0 {
            let _ = SetupDiDestroyDeviceInfoList(dev_info);
            bail!("SetupDiGetDeviceInterfaceDetailW returned zero size");
        }

        let mut buf = vec![0u8; required as usize];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        let ok = SetupDiGetDeviceInterfaceDetailW(
            dev_info,
            &iface,
            Some(detail),
            required,
            None,
            None,
        );
        let _ = SetupDiDestroyDeviceInfoList(dev_info);
        ok.context("SetupDiGetDeviceInterfaceDetailW")?;

        let path_ptr = &(*detail).DevicePath as *const u16;
        let handle = CreateFileW(
            PCWSTR(path_ptr),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .context("CreateFileW virtual display device")?;
        Ok(OwnedHandle(handle))
    }
}

fn zako_ping() -> bool {
    let dev = match open_device_interface(&GUID_ZAKO_VDD) {
        Ok(d) => d,
        Err(e) => {
            log!("vdisplay: ZakoVDD open failed: {e:#}");
            return false;
        }
    };
    let mut returned: u32 = 0;
    let res = unsafe {
        DeviceIoControl(
            dev.0,
            IOCTL_ZAKO_PING,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    };
    if let Err(e) = res {
        log!("vdisplay: ZakoVDD ping ioctl failed: {e:#}");
        return false;
    }
    true
}

fn zako_send_command(cmd: &str) -> Result<()> {
    let dev = open_device_interface(&GUID_ZAKO_VDD)?;
    let mut wide: Vec<u16> = cmd.encode_utf16().collect();
    wide.push(0);
    let bytes = (wide.len() * size_of::<u16>()) as u32;
    let mut returned: u32 = 0;
    unsafe {
        DeviceIoControl(
            dev.0,
            IOCTL_ZAKO_COMMAND,
            Some(wide.as_ptr() as *const c_void),
            bytes,
            None,
            0,
            Some(&mut returned),
            None,
        )
        .with_context(|| format!("ZakoVDD IOCTL_VDD_COMMAND({cmd})"))?;
    }
    Ok(())
}

fn sudo_check_version() -> bool {
    let Ok(dev) = open_device_interface(&GUID_SUDOVDA) else {
        return false;
    };
    let mut out = SudoVersionOut::default();
    let mut returned: u32 = 0;
    unsafe {
        DeviceIoControl(
            dev.0,
            IOCTL_SUDO_VERSION,
            None,
            0,
            Some(&mut out as *mut _ as *mut c_void),
            size_of::<SudoVersionOut>() as u32,
            Some(&mut returned),
            None,
        )
        .is_ok()
    }
}

fn sudo_ping() -> Result<()> {
    let dev = open_device_interface(&GUID_SUDOVDA)?;
    let mut returned: u32 = 0;
    unsafe {
        DeviceIoControl(dev.0, IOCTL_SUDO_PING, None, 0, None, 0, Some(&mut returned), None)?;
    }
    Ok(())
}

fn sudo_add_display(w: u32, h: u32, hz: u32, guid: GUID) -> Result<SudoAddOut> {
    let dev = open_device_interface(&GUID_SUDOVDA)?;
    let mut name = [0u8; 14];
    let n = b"RCPhone";
    name[..n.len()].copy_from_slice(n);
    let mut serial = [0u8; 14];
    let s = b"RCPHONE01";
    serial[..s.len()].copy_from_slice(s);
    let params = SudoAddParams {
        width: w,
        height: h,
        refresh_rate: hz,
        monitor_guid: guid,
        device_name: name,
        serial_number: serial,
    };
    let mut out = SudoAddOut::default();
    let mut returned: u32 = 0;
    unsafe {
        DeviceIoControl(
            dev.0,
            IOCTL_SUDO_ADD,
            Some(&params as *const _ as *const c_void),
            size_of::<SudoAddParams>() as u32,
            Some(&mut out as *mut _ as *mut c_void),
            size_of::<SudoAddOut>() as u32,
            Some(&mut returned),
            None,
        )
        .context("SudoVDA IOCTL_ADD_VIRTUAL_DISPLAY")?;
    }
    Ok(out)
}

fn sudo_remove_display(guid: GUID) -> Result<()> {
    let dev = open_device_interface(&GUID_SUDOVDA)?;
    let params = SudoRemoveParams { monitor_guid: guid };
    let mut returned: u32 = 0;
    unsafe {
        DeviceIoControl(
            dev.0,
            IOCTL_SUDO_REMOVE,
            Some(&params as *const _ as *const c_void),
            size_of::<SudoRemoveParams>() as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
        .context("SudoVDA IOCTL_REMOVE_VIRTUAL_DISPLAY")?;
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ActivePath {
    device_name: String,
    friendly_name: String,
    adapter_luid: LUID,
    source_id: u32,
    target_id: u32,
}

fn list_active_paths() -> Vec<ActivePath> {
    unsafe {
        let flags = QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE;
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        if GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) != ERROR_SUCCESS {
            return Vec::new();
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        if QueryDisplayConfig(
            flags,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return Vec::new();
        }
        paths.truncate(path_count as usize);

        let mut out = Vec::with_capacity(paths.len());
        for p in &paths {
            let mut src_name = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: p.sourceInfo.adapterId,
                    id: p.sourceInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut src_name.header) != ERROR_SUCCESS.0 as i32 {
                continue;
            }
            let len = src_name
                .viewGdiDeviceName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(src_name.viewGdiDeviceName.len());
            let device_name = String::from_utf16_lossy(&src_name.viewGdiDeviceName[..len]);

            let mut tgt_name = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                    size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                    adapterId: p.targetInfo.adapterId,
                    id: p.targetInfo.id,
                },
                ..Default::default()
            };
            let friendly_name = if DisplayConfigGetDeviceInfo(&mut tgt_name.header) == ERROR_SUCCESS.0 as i32 {
                let flen = tgt_name
                    .monitorFriendlyDeviceName
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(tgt_name.monitorFriendlyDeviceName.len());
                String::from_utf16_lossy(&tgt_name.monitorFriendlyDeviceName[..flen])
            } else {
                String::new()
            };

            out.push(ActivePath {
                device_name,
                friendly_name,
                adapter_luid: p.sourceInfo.adapterId,
                source_id: p.sourceInfo.id,
                target_id: p.targetInfo.id,
            });
        }
        out
    }
}

fn find_path_by_device_name(name: &str) -> Option<ActivePath> {
    list_active_paths()
        .into_iter()
        .find(|p| p.device_name.eq_ignore_ascii_case(name))
}

fn find_zako_path() -> Option<ActivePath> {
    list_active_paths().into_iter().find(|p| {
        let f = p.friendly_name.to_ascii_lowercase();
        f.contains("zako") || f.contains("vdd") || f.contains("sunshine")
    })
}

fn wait_for_path<F: Fn() -> Option<ActivePath>>(timeout: Duration, check: F) -> Option<ActivePath> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(p) = check() {
            return Some(p);
        }
        thread::sleep(Duration::from_millis(50));
    }
    check()
}

fn query_display_rect(device_name: &str) -> Option<RECT> {
    let mut dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let wide = HSTRING::from(device_name);
    let ok = unsafe { EnumDisplaySettingsW(&wide, ENUM_CURRENT_SETTINGS, &mut dm) }.as_bool();
    if !ok || dm.dmPelsWidth == 0 || dm.dmPelsHeight == 0 {
        return None;
    }
    let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
    Some(RECT {
        left: pos.x,
        top: pos.y,
        right: pos.x + dm.dmPelsWidth as i32,
        bottom: pos.y + dm.dmPelsHeight as i32,
    })
}

fn rightmost_other_display_x(exclude_name: &str) -> i32 {
    let mut max_right = 0i32;
    for p in list_active_paths() {
        if p.device_name.eq_ignore_ascii_case(exclude_name) {
            continue;
        }
        if let Some(r) = query_display_rect(&p.device_name) {
            max_right = max_right.max(r.right);
        }
    }
    max_right.max(1920)
}

fn apply_display_mode(device_name: &str, w: u32, h: u32, hz: u32, x: i32, y: i32) -> Result<()> {
    let wide = HSTRING::from(device_name);
    let mut dm = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let _ = unsafe { EnumDisplaySettingsW(&wide, ENUM_CURRENT_SETTINGS, &mut dm) };
    dm.dmSize = size_of::<DEVMODEW>() as u16;
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_POSITION;
    dm.dmPelsWidth = w;
    dm.dmPelsHeight = h;
    dm.dmDisplayFrequency = hz;
    dm.Anonymous1 = DEVMODEW_0 {
        Anonymous2: DEVMODEW_0_1 {
            dmPosition: POINTL { x, y },
            dmDisplayOrientation: Default::default(),
            dmDisplayFixedOutput: Default::default(),
        },
    };

    let r1 = unsafe {
        ChangeDisplaySettingsExW(
            &wide,
            Some(&dm),
            None,
            CDS_UPDATEREGISTRY | CDS_NORESET,
            None,
        )
    };
    let r2 = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) };
    if r1 != DISP_CHANGE_SUCCESSFUL || r2 != DISP_CHANGE_SUCCESSFUL {
        // Fallback: retry without forcing frequency (some drivers advertise 60Hz as 59/60).
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION;
        let r3 = unsafe {
            ChangeDisplaySettingsExW(&wide, Some(&dm), None, CDS_UPDATEREGISTRY | CDS_NORESET, None)
        };
        let r4 = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) };
        if r3 != DISP_CHANGE_SUCCESSFUL || r4 != DISP_CHANGE_SUCCESSFUL {
            bail!("ChangeDisplaySettingsExW({device_name} {w}x{h}) failed: {r1:?}/{r2:?} retry {r3:?}/{r4:?}");
        }
    }
    Ok(())
}

/// Immediately sets per-monitor DPI scaling (`target_percent` in `DPI_SCALE_STEPS`)
/// using Windows' `DISPLAYCONFIG_DEVICE_INFO_SET_DPI_SCALE` packet.
pub fn set_monitor_dpi(adapter_luid: LUID, source_id: u32, target_percent: u16) -> bool {
    let mut get_pkt = DisplayConfigDpiGet {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(DISPLAYCONFIG_DEVICE_INFO_GET_DPI_SCALE),
            size: size_of::<DisplayConfigDpiGet>() as u32,
            adapterId: adapter_luid,
            id: source_id,
        },
        min_scale_rel: 0,
        cur_scale_rel: 0,
        max_scale_rel: 0,
    };
    let gr = unsafe { DisplayConfigGetDeviceInfo(&mut get_pkt.header) };
    if gr != ERROR_SUCCESS.0 as i32 {
        return false;
    }

    let recommended_idx = get_pkt.min_scale_rel.unsigned_abs() as i32;
    let Some(target_idx) = DPI_SCALE_STEPS
        .iter()
        .position(|&v| v == target_percent)
        .map(|i| i as i32)
    else {
        return false;
    };

    let rel = (target_idx - recommended_idx).clamp(get_pkt.min_scale_rel, get_pkt.max_scale_rel);
    let mut set_pkt = DisplayConfigDpiSet {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(DISPLAYCONFIG_DEVICE_INFO_SET_DPI_SCALE),
            size: size_of::<DisplayConfigDpiSet>() as u32,
            adapterId: adapter_luid,
            id: source_id,
        },
        scale_rel: rel,
    };
    unsafe { DisplayConfigSetDeviceInfo(&mut set_pkt.header) == ERROR_SUCCESS.0 as i32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommended_dpi_matches_form_factors() {
        assert_eq!(VirtualDisplayManager::recommended_dpi(1080, 2412), 225);
        assert_eq!(VirtualDisplayManager::recommended_dpi(2412, 1080), 175);
        assert_eq!(VirtualDisplayManager::recommended_dpi(2560, 1600), 150);
    }

    #[test]
    fn detects_local_vdisplay_backend() {
        let mgr = VirtualDisplayManager::new();
        println!("detected vdisplay backend: {:?}", mgr.backend);
    }
}
