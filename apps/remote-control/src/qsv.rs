//! Native Intel Quick Sync Video (oneVPL / Media SDK) hardware encoder over
//! D3D11 textures, dynamically loaded from the Intel graphics driver.
//!
//! Intel's Media Foundation wrapper (`mfx_mft_h264ve_64.dll`) adds ~10–14 ms
//! of fixed per-frame overhead on top of the hardware encode (capping even an
//! Arc iGPU to ~67 fps at 2160x1440). Calling oneVPL directly with zero-copy
//! D3D11 textures bypasses that wrapper while requiring zero bundled DLLs: if
//! the adapter is not Intel or the driver runtime is unavailable, callers fall
//! back seamlessly to Media Foundation (`encode.rs`).

use std::{
    ffi::{CStr, c_void},
    mem::size_of,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use windows::{
    Win32::{
        Foundation::{HMODULE, LUID},
        Graphics::{
            Direct3D11::{
                D3D11_BIND_DECODER, D3D11_BIND_RENDER_TARGET, D3D11_BOX, D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ,
                D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
                D3D11_USAGE_STAGING, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
            },
            Dxgi::{
                Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC},
                CreateDXGIFactory1, IDXGIFactory1,
            },
        },
        System::{
            LibraryLoader::{GetProcAddress, LoadLibraryW},
            Registry::{HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW},
        },
    },
    core::{HSTRING, Interface, PCSTR, PWSTR},
};

use crate::{
    encode::{EncOptions, Encoded},
    mf::Codec,
};

// ---- oneVPL / Media SDK ABI constants and structures (x86_64 Windows) ----

type MfxStatus = i32;
type MfxSession = *mut c_void;
type MfxLoader = *mut c_void;
type MfxConfig = *mut c_void;
type MfxSyncPoint = *mut c_void;
type MfxMemId = *mut c_void;
type MfxHdl = *mut c_void;

const MFX_ERR_NONE: MfxStatus = 0;
const MFX_ERR_NULL_PTR: MfxStatus = -2;
const MFX_ERR_UNSUPPORTED: MfxStatus = -3;
const MFX_ERR_MEMORY_ALLOC: MfxStatus = -4;
const MFX_ERR_NOT_ENOUGH_BUFFER: MfxStatus = -5;
const MFX_ERR_MORE_DATA: MfxStatus = -10;
const MFX_WRN_IN_EXECUTION: MfxStatus = 1;
const MFX_WRN_DEVICE_BUSY: MfxStatus = 2;

const MFX_IMPL_TYPE_HARDWARE: u32 = 0x0002;
const MFX_ACCEL_MODE_VIA_D3D11: u32 = 0x0300;
const MFX_VARIANT_TYPE_U32: u32 = 5;

const MFX_HANDLE_D3D11_DEVICE: i32 = 3;

const MFX_IOPATTERN_IN_VIDEO_MEMORY: u16 = 0x01;
const MFX_PICSTRUCT_PROGRESSIVE: u16 = 0x01;
const MFX_CHROMAFORMAT_YUV420: u16 = 1;

const MFX_RATECONTROL_CBR: u16 = 1;
const MFX_RATECONTROL_CQP: u16 = 3;

const MFX_TARGETUSAGE_BEST_QUALITY: u16 = 1;
const MFX_TARGETUSAGE_BALANCED: u16 = 4;
const MFX_TARGETUSAGE_BEST_SPEED: u16 = 7;

const MFX_CODINGOPTION_ON: u16 = 0x10;
const MFX_CODINGOPTION_OFF: u16 = 0x20;

const MFX_PROFILE_AVC_HIGH: u16 = 100;
const MFX_PROFILE_HEVC_MAIN: u16 = 1;

const MFX_FRAMETYPE_I: u16 = 0x0001;
const MFX_FRAMETYPE_REF: u16 = 0x0040;
const MFX_FRAMETYPE_IDR: u16 = 0x0080;

const MFX_MEMTYPE_VIDEO_MEMORY_DECODER_TARGET: u16 = 0x0010;
const MFX_MEMTYPE_VIDEO_MEMORY_PROCESSOR_TARGET: u16 = 0x0020;
const MFX_MEMTYPE_SYSTEM_MEMORY: u16 = 0x0040;
const MFX_MEMTYPE_EXTERNAL_FRAME: u16 = 0x0002;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

const MFX_CODEC_AVC: u32 = fourcc(b'A', b'V', b'C', b' ');
const MFX_CODEC_HEVC: u32 = fourcc(b'H', b'E', b'V', b'C');
const MFX_FOURCC_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
const MFX_FOURCC_P8: u32 = fourcc(b'P', b'8', b' ', b' ');
const MFX_EXTBUFF_CODING_OPTION: u32 = fourcc(b'C', b'D', b'O', b'P');
const MFX_EXTBUFF_VIDEO_SIGNAL_INFO: u32 = fourcc(b'V', b'S', b'I', b'N');
const MFX_EXTBUFF_CODING_OPTION3: u32 = fourcc(b'C', b'D', b'O', b'3');
const MFX_SCENARIO_DISPLAY_REMOTING: u16 = 7;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MfxVariant {
    version: u16,
    _pad: u16,
    ty: u32,
    data: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MfxExtBuffer {
    buffer_id: u32,
    buffer_sz: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MfxHdlPair {
    first: MfxHdl,
    second: MfxHdl,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MfxFrameId {
    temporal_id: u16,
    priority_id: u16,
    dependency_id: u16,
    quality_id: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MfxFrameInfo {
    reserved: [u32; 4],
    channel_id: u16,
    bit_depth_luma: u16,
    bit_depth_chroma: u16,
    shift: u16,
    frame_id: MfxFrameId,
    fourcc: u32,
    width: u16,
    height: u16,
    crop_x: u16,
    crop_y: u16,
    crop_w: u16,
    crop_h: u16,
    frame_rate_ext_n: u32,
    frame_rate_ext_d: u32,
    reserved3: u16,
    aspect_ratio_w: u16,
    aspect_ratio_h: u16,
    pic_struct: u16,
    chroma_format: u16,
    reserved2: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MfxFrameData {
    ext_param: *mut *mut MfxExtBuffer,
    num_ext_param: u16,
    reserved: [u16; 9],
    mem_type: u16,
    pitch_high: u16,
    time_stamp: u64,
    frame_order: u32,
    locked: u16,
    pitch_low: u16,
    y: *mut u8,
    uv: *mut u8,
    v: *mut u8,
    a: *mut u8,
    mem_id: MfxMemId,
    corrupted: u16,
    data_flag: u16,
}

impl Default for MfxFrameData {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MfxFrameSurface1 {
    frame_interface: *mut c_void,
    version: u16,
    reserved1: [u16; 3],
    info: MfxFrameInfo,
    data: MfxFrameData,
}

impl Default for MfxFrameSurface1 {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MfxInfoMfx {
    reserved: [u32; 7],
    low_power: u16,
    brc_param_multiplier: u16,
    frame_info: MfxFrameInfo,
    codec_id: u32,
    codec_profile: u16,
    codec_level: u16,
    num_thread: u16,
    target_usage: u16,
    gop_pic_size: u16,
    gop_ref_dist: u16,
    gop_opt_flag: u16,
    idr_interval: u16,
    rate_control_method: u16,
    initial_delay_in_kb: u16,
    buffer_size_in_kb: u16,
    target_kbps: u16,
    max_kbps: u16,
    num_slice: u16,
    num_ref_frame: u16,
    encoded_order: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MfxVideoParam {
    alloc_id: u32,
    reserved: [u32; 2],
    reserved3: u16,
    async_depth: u16,
    mfx: MfxInfoMfx,
    /// Padding to match the larger `mfxInfoVPP` arm (168 bytes) of the C union.
    _vpp_union_pad: [u8; 32],
    protected: u16,
    io_pattern: u16,
    ext_param: *mut *mut MfxExtBuffer,
    num_ext_param: u16,
    reserved2: u16,
}

impl Default for MfxVideoParam {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MfxBitstream {
    reserved: [u32; 6],
    decode_time_stamp: i64,
    time_stamp: u64,
    data: *mut u8,
    data_offset: u32,
    data_length: u32,
    max_length: u32,
    pic_struct: u16,
    frame_type: u16,
    data_flag: u16,
    reserved2: u16,
}

impl Default for MfxBitstream {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MfxEncodeCtrl {
    header: MfxExtBuffer,
    reserved: [u32; 4],
    reserved1: u16,
    mfx_nal_unit_type: u16,
    skip_frame: u16,
    qp: u16,
    frame_type: u16,
    num_ext_param: u16,
    num_payload: u16,
    reserved2: u16,
    ext_param: *mut *mut MfxExtBuffer,
    payload: *mut *mut c_void,
}

impl Default for MfxEncodeCtrl {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MfxExtCodingOption {
    header: MfxExtBuffer,
    reserved1: u16,
    rate_distortion_opt: u16,
    me_cost_type: u16,
    me_search_type: u16,
    mv_search_window: [i16; 2],
    end_of_sequence: u16,
    frame_picture: u16,
    cavlc: u16,
    reserved2: [u16; 2],
    recovery_point_sei: u16,
    view_output: u16,
    nal_hrd_conformance: u16,
    single_sei_nal_unit: u16,
    vui_vcl_hrd_parameters: u16,
    ref_pic_list_reordering: u16,
    reset_ref_list: u16,
    ref_pic_mark_rep: u16,
    field_output: u16,
    intra_pred_block_size: u16,
    inter_pred_block_size: u16,
    mv_precision: u16,
    max_dec_frame_buffering: u16,
    au_delimiter: u16,
    end_of_stream: u16,
    pic_timing_sei: u16,
    vui_nal_hrd_parameters: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MfxExtVideoSignalInfo {
    header: MfxExtBuffer,
    video_format: u16,
    video_full_range: u16,
    colour_description_present: u16,
    colour_primaries: u16,
    transfer_characteristics: u16,
    matrix_coefficients: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MfxExtCodingOption3 {
    header: MfxExtBuffer,
    num_slice_i: u16,
    num_slice_p: u16,
    num_slice_b: u16,
    win_brc_max_avg_kbps: u16,
    win_brc_size: u16,
    qvbr_quality: u16,
    enable_mbqp: u16,
    int_ref_cycle_dist: u16,
    direct_bias_adjustment: u16,
    global_motion_bias_adjustment: u16,
    mv_cost_scaling_factor: u16,
    mb_disable_skip_map: u16,
    weighted_pred: u16,
    weighted_bi_pred: u16,
    aspect_ratio_info_present: u16,
    overscan_info_present: u16,
    overscan_appropriate: u16,
    timing_info_present: u16,
    bitstream_restriction: u16,
    low_delay_hrd: u16,
    motion_vectors_over_pic_boundaries: u16,
    reserved1: [u16; 2],
    scenario_info: u16,
    content_info: u16,
    p_ref_type: u16,
    fade_detection: u16,
    reserved2: [u16; 2],
    gpb: u16,
    max_frame_size_i: u32,
    max_frame_size_p: u32,
    reserved3: [u32; 3],
    enable_qp_offset: u16,
    qp_offset: [i16; 8],
    num_ref_active_p: [u16; 8],
    num_ref_active_bl0: [u16; 8],
    num_ref_active_bl1: [u16; 8],
    reserved6: u16,
    transform_skip: u16,
    target_chroma_format_plus1: u16,
    target_bit_depth_luma: u16,
    target_bit_depth_chroma: u16,
    brc_panic_mode: u16,
    low_delay_brc: u16,
    enable_mb_force_intra: u16,
    adaptive_max_frame_size: u16,
    repartition_check_enable: u16,
    reserved5: [u16; 3],
    encoded_units_info: u16,
    enable_nal_unit_type: u16,
    adaptive_ltr: u16,
    adaptive_cqm: u16,
    adaptive_ref: u16,
    reserved: [u16; 161],
}

impl Default for MfxExtCodingOption3 {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MfxFrameAllocRequest {
    alloc_id: u32,
    reserved3: [u32; 3],
    info: MfxFrameInfo,
    ty: u16,
    num_frame_min: u16,
    num_frame_suggested: u16,
    reserved2: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MfxFrameAllocResponse {
    alloc_id: u32,
    reserved: [u32; 3],
    mids: *mut MfxMemId,
    num_frame_actual: u16,
    reserved2: u16,
}

#[repr(C)]
struct MfxFrameAllocator {
    reserved: [u32; 4],
    pthis: MfxHdl,
    alloc: unsafe extern "C" fn(MfxHdl, *mut MfxFrameAllocRequest, *mut MfxFrameAllocResponse) -> MfxStatus,
    lock: unsafe extern "C" fn(MfxHdl, MfxMemId, *mut MfxFrameData) -> MfxStatus,
    unlock: unsafe extern "C" fn(MfxHdl, MfxMemId, *mut MfxFrameData) -> MfxStatus,
    get_hdl: unsafe extern "C" fn(MfxHdl, MfxMemId, *mut MfxHdl) -> MfxStatus,
    free: unsafe extern "C" fn(MfxHdl, *mut MfxFrameAllocResponse) -> MfxStatus,
}

const _: () = {
    assert!(size_of::<MfxVariant>() == 16);
    assert!(size_of::<MfxFrameInfo>() == 68);
    assert!(size_of::<MfxInfoMfx>() == 136);
    assert!(size_of::<MfxVideoParam>() == 208);
    assert!(size_of::<MfxFrameData>() == 96);
    assert!(size_of::<MfxFrameSurface1>() == 184);
    assert!(size_of::<MfxBitstream>() == 72);
    assert!(size_of::<MfxEncodeCtrl>() == 56);
    assert!(size_of::<MfxExtCodingOption>() == 64);
    assert!(size_of::<MfxExtVideoSignalInfo>() == 20);
    assert!(size_of::<MfxExtCodingOption3>() == 512);
    assert!(size_of::<MfxFrameAllocRequest>() == 92);
    assert!(size_of::<MfxFrameAllocResponse>() == 32);
    assert!(size_of::<MfxFrameAllocator>() == 64);
};

// ---- Dynamic loader for libvpl.dll / vpl_dispatcher_64.dll ----

struct VplApi {
    _module: HMODULE,
    mfx_load: unsafe extern "C" fn() -> MfxLoader,
    mfx_unload: unsafe extern "C" fn(MfxLoader),
    mfx_create_config: unsafe extern "C" fn(MfxLoader) -> MfxConfig,
    mfx_set_config_filter_property: unsafe extern "C" fn(MfxConfig, *const u8, MfxVariant) -> MfxStatus,
    mfx_create_session: unsafe extern "C" fn(MfxLoader, u32, *mut MfxSession) -> MfxStatus,
    mfx_close: unsafe extern "C" fn(MfxSession) -> MfxStatus,
    set_handle: unsafe extern "C" fn(MfxSession, i32, MfxHdl) -> MfxStatus,
    set_frame_allocator: unsafe extern "C" fn(MfxSession, *mut MfxFrameAllocator) -> MfxStatus,
    sync_operation: unsafe extern "C" fn(MfxSession, MfxSyncPoint, u32) -> MfxStatus,
    encode_init: unsafe extern "C" fn(MfxSession, *mut MfxVideoParam) -> MfxStatus,
    encode_reset: unsafe extern "C" fn(MfxSession, *mut MfxVideoParam) -> MfxStatus,
    encode_close: unsafe extern "C" fn(MfxSession) -> MfxStatus,
    encode_get_video_param: unsafe extern "C" fn(MfxSession, *mut MfxVideoParam) -> MfxStatus,
    encode_frame_async: unsafe extern "C" fn(
        MfxSession,
        *mut MfxEncodeCtrl,
        *mut MfxFrameSurface1,
        *mut MfxBitstream,
        *mut MfxSyncPoint,
    ) -> MfxStatus,
}

unsafe impl Send for VplApi {}
unsafe impl Sync for VplApi {}

static VPL: OnceLock<Option<VplApi>> = OnceLock::new();

/// A configured oneVPL loader, by codec and adapter (`None` = any adapter).
/// The first session from a loader takes 200-250 ms (the dispatcher finds and
/// loads the runtime; Arc, 2026-11-24), later ones from the same loader are
/// quick, so loaders stay for the life of the process: every new picture size
/// or frame rate builds a new encoder.
struct CachedLoader {
    codec_id: u32,
    adapter: Option<u32>,
    loader: MfxLoader,
}

unsafe impl Send for CachedLoader {}

static LOADERS: Mutex<Vec<CachedLoader>> = Mutex::new(Vec::new());

fn vpl_api() -> Option<&'static VplApi> {
    VPL.get_or_init(load_vpl).as_ref()
}

fn load_vpl() -> Option<VplApi> {
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("libvpl.dll")];
    for dir in intel_vpl_driver_dirs() {
        candidates.push(dir.join("vpl_dispatcher_64.dll"));
    }
    for path in candidates {
        if let Some(api) = try_load_vpl(&path) {
            return Some(api);
        }
    }
    None
}

fn try_load_vpl(path: &std::path::Path) -> Option<VplApi> {
    unsafe {
        let h = LoadLibraryW(&HSTRING::from(path.as_os_str())).ok()?;
        macro_rules! sym {
            ($name:literal, $ty:ty) => {{
                let c = CStr::from_bytes_with_nul_unchecked(concat!($name, "\0").as_bytes());
                let p = GetProcAddress(h, PCSTR(c.as_ptr() as *const u8))?;
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(p)
            }};
        }
        Some(VplApi {
            _module: h,
            mfx_load: sym!("MFXLoad", unsafe extern "C" fn() -> MfxLoader),
            mfx_unload: sym!("MFXUnload", unsafe extern "C" fn(MfxLoader)),
            mfx_create_config: sym!("MFXCreateConfig", unsafe extern "C" fn(MfxLoader) -> MfxConfig),
            mfx_set_config_filter_property: sym!(
                "MFXSetConfigFilterProperty",
                unsafe extern "C" fn(MfxConfig, *const u8, MfxVariant) -> MfxStatus
            ),
            mfx_create_session: sym!("MFXCreateSession", unsafe extern "C" fn(MfxLoader, u32, *mut MfxSession) -> MfxStatus),
            mfx_close: sym!("MFXClose", unsafe extern "C" fn(MfxSession) -> MfxStatus),
            set_handle: sym!("MFXVideoCORE_SetHandle", unsafe extern "C" fn(MfxSession, i32, MfxHdl) -> MfxStatus),
            set_frame_allocator: sym!(
                "MFXVideoCORE_SetFrameAllocator",
                unsafe extern "C" fn(MfxSession, *mut MfxFrameAllocator) -> MfxStatus
            ),
            sync_operation: sym!(
                "MFXVideoCORE_SyncOperation",
                unsafe extern "C" fn(MfxSession, MfxSyncPoint, u32) -> MfxStatus
            ),
            encode_init: sym!("MFXVideoENCODE_Init", unsafe extern "C" fn(MfxSession, *mut MfxVideoParam) -> MfxStatus),
            encode_reset: sym!("MFXVideoENCODE_Reset", unsafe extern "C" fn(MfxSession, *mut MfxVideoParam) -> MfxStatus),
            encode_close: sym!("MFXVideoENCODE_Close", unsafe extern "C" fn(MfxSession) -> MfxStatus),
            encode_get_video_param: sym!(
                "MFXVideoENCODE_GetVideoParam",
                unsafe extern "C" fn(MfxSession, *mut MfxVideoParam) -> MfxStatus
            ),
            encode_frame_async: sym!(
                "MFXVideoENCODE_EncodeFrameAsync",
                unsafe extern "C" fn(
                    MfxSession,
                    *mut MfxEncodeCtrl,
                    *mut MfxFrameSurface1,
                    *mut MfxBitstream,
                    *mut MfxSyncPoint,
                ) -> MfxStatus
            ),
        })
    }
}

/// Finds `DriverStorePathForVPL` values under the Display Adapters registry class.
fn intel_vpl_driver_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let base = HSTRING::from(r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}");
    unsafe {
        let mut root = HKEY::default();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, &base, Some(0), KEY_READ, &mut root).is_err() {
            return dirs;
        }
        let mut idx = 0u32;
        let mut name = [0u16; 64];
        loop {
            let mut len = name.len() as u32;
            if RegEnumKeyExW(root, idx, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None).is_err() {
                break;
            }
            idx += 1;
            let sub_name = HSTRING::from(String::from_utf16_lossy(&name[..len as usize]));
            let mut sub = HKEY::default();
            if RegOpenKeyExW(root, &sub_name, Some(0), KEY_READ, &mut sub).is_ok() {
                let mut ty = REG_SZ;
                let mut buf = [0u8; 1024];
                let mut bytes = buf.len() as u32;
                let val_name = HSTRING::from("DriverStorePathForVPL");
                if RegQueryValueExW(sub, &val_name, None, Some(&mut ty), Some(buf.as_mut_ptr()), Some(&mut bytes)).is_ok()
                    && ty == REG_SZ
                    && bytes >= 2
                {
                    let u16s: Vec<u16> = buf[..bytes as usize]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .take_while(|&w| w != 0)
                        .collect();
                    let p = PathBuf::from(String::from_utf16_lossy(&u16s));
                    if !p.as_os_str().is_empty() && !dirs.contains(&p) {
                        dirs.push(p);
                    }
                }
                let _ = RegCloseKey(sub);
            }
        }
        let _ = RegCloseKey(root);
    }
    dirs
}

fn intel_adapter_index(luid: LUID) -> Option<u32> {
    unsafe {
        let factory = CreateDXGIFactory1::<IDXGIFactory1>().ok()?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            if let Ok(d) = adapter.GetDesc1()
                && d.AdapterLuid.LowPart == luid.LowPart
                && d.AdapterLuid.HighPart == luid.HighPart
            {
                return (d.VendorId == 0x8086).then_some(i);
            }
            i += 1;
        }
        None
    }
}

/// True when `luid` belongs to an Intel graphics adapter (`VendorId == 0x8086`).
pub fn is_intel_adapter(luid: LUID) -> bool {
    intel_adapter_index(luid).is_some()
}

/// True when `luid` is an Intel GPU and the driver's oneVPL runtime is loadable.
pub fn is_available(luid: LUID) -> bool {
    is_intel_adapter(luid) && vpl_api().is_some()
}

// ---- D3D11 mfxFrameAllocator ----

enum AllocResource {
    Tex(ID3D11Texture2D),
    Buf(ID3D11Buffer),
    /// External borrowed texture (raw pointer only; caller owns lifetime).
    External(*mut c_void),
}

struct MidEntry {
    res: AllocResource,
    is_p8: bool,
}

impl MidEntry {
    fn raw_ptr(&self) -> *mut c_void {
        match &self.res {
            AllocResource::Tex(t) => t.as_raw(),
            AllocResource::Buf(b) => b.as_raw(),
            AllocResource::External(p) => *p,
        }
    }
}

struct AllocBatch {
    mids: Vec<MfxMemId>,
    // FFI holds raw pointers to each MidEntry across batch allocations.
    #[allow(clippy::vec_box)]
    _entries: Vec<Box<MidEntry>>,
}

struct D3D11Allocator {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    batches: Vec<AllocBatch>,
}

unsafe extern "C" fn alloc_frames(
    pthis: MfxHdl,
    req: *mut MfxFrameAllocRequest,
    resp: *mut MfxFrameAllocResponse,
) -> MfxStatus {
    unsafe {
        if pthis.is_null() || req.is_null() || resp.is_null() {
            return MFX_ERR_NULL_PTR;
        }
        let state = &mut *(pthis as *mut D3D11Allocator);
        let req = &*req;
        if req.ty & MFX_MEMTYPE_SYSTEM_MEMORY != 0 {
            return MFX_ERR_UNSUPPORTED;
        }
        let n = req.num_frame_suggested.max(req.num_frame_min).max(1) as usize;
        let mut entries = Vec::with_capacity(n);
        let mut mids = Vec::with_capacity(n);

        if req.info.fourcc == MFX_FOURCC_P8 {
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: (req.info.width as u32) * (req.info.height as u32),
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                StructureByteStride: 0,
            };
            for _ in 0..n {
                let mut buf: Option<ID3D11Buffer> = None;
                if state.device.CreateBuffer(&desc, None, Some(&mut buf)).is_err() {
                    return MFX_ERR_MEMORY_ALLOC;
                }
                let Some(buf) = buf else { return MFX_ERR_MEMORY_ALLOC };
                let mut b = Box::new(MidEntry { res: AllocResource::Buf(buf), is_p8: true });
                mids.push((&mut *b as *mut MidEntry) as MfxMemId);
                entries.push(b);
            }
        } else if req.info.fourcc == MFX_FOURCC_NV12 {
            let bind = if req.ty & MFX_MEMTYPE_VIDEO_MEMORY_PROCESSOR_TARGET != 0 {
                D3D11_BIND_RENDER_TARGET.0 as u32
            } else {
                D3D11_BIND_DECODER.0 as u32
            };
            let desc = D3D11_TEXTURE2D_DESC {
                Width: req.info.width as u32,
                Height: req.info.height as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: bind,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            for _ in 0..n {
                let mut tex: Option<ID3D11Texture2D> = None;
                if state.device.CreateTexture2D(&desc, None, Some(&mut tex)).is_err() {
                    return MFX_ERR_MEMORY_ALLOC;
                }
                let Some(tex) = tex else { return MFX_ERR_MEMORY_ALLOC };
                let mut b = Box::new(MidEntry { res: AllocResource::Tex(tex), is_p8: false });
                mids.push((&mut *b as *mut MidEntry) as MfxMemId);
                entries.push(b);
            }
        } else {
            return MFX_ERR_UNSUPPORTED;
        }

        let mids_ptr = mids.as_mut_ptr();
        state.batches.push(AllocBatch { mids, _entries: entries });
        (*resp).mids = mids_ptr;
        (*resp).num_frame_actual = n as u16;
        MFX_ERR_NONE
    }
}

unsafe extern "C" fn lock_frame(pthis: MfxHdl, mid: MfxMemId, ptr: *mut MfxFrameData) -> MfxStatus {
    unsafe {
        if pthis.is_null() || mid.is_null() || ptr.is_null() {
            return MFX_ERR_NULL_PTR;
        }
        let state = &*(pthis as *const D3D11Allocator);
        let entry = &*(mid as *const MidEntry);
        if let AllocResource::Buf(buf) = &entry.res
            && entry.is_p8
        {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if state.ctx.Map(buf, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).is_err() {
                return MFX_ERR_UNSUPPORTED;
            }
            (*ptr).y = mapped.pData as *mut u8;
            (*ptr).pitch_low = mapped.RowPitch as u16;
            (*ptr).pitch_high = (mapped.RowPitch >> 16) as u16;
            return MFX_ERR_NONE;
        }
        MFX_ERR_UNSUPPORTED
    }
}

unsafe extern "C" fn unlock_frame(pthis: MfxHdl, mid: MfxMemId, ptr: *mut MfxFrameData) -> MfxStatus {
    unsafe {
        if pthis.is_null() || mid.is_null() {
            return MFX_ERR_NULL_PTR;
        }
        let state = &*(pthis as *const D3D11Allocator);
        let entry = &*(mid as *const MidEntry);
        if let AllocResource::Buf(buf) = &entry.res
            && entry.is_p8
        {
            state.ctx.Unmap(buf, 0);
            if !ptr.is_null() {
                (*ptr).y = std::ptr::null_mut();
                (*ptr).pitch_low = 0;
                (*ptr).pitch_high = 0;
            }
            return MFX_ERR_NONE;
        }
        MFX_ERR_UNSUPPORTED
    }
}

unsafe extern "C" fn get_hdl(_pthis: MfxHdl, mid: MfxMemId, handle: *mut MfxHdl) -> MfxStatus {
    unsafe {
        if mid.is_null() || handle.is_null() {
            return MFX_ERR_NULL_PTR;
        }
        let entry = &*(mid as *const MidEntry);
        let pair = &mut *(handle as *mut MfxHdlPair);
        pair.first = entry.raw_ptr();
        pair.second = std::ptr::null_mut();
        MFX_ERR_NONE
    }
}

unsafe extern "C" fn free_frames(pthis: MfxHdl, resp: *mut MfxFrameAllocResponse) -> MfxStatus {
    unsafe {
        if pthis.is_null() || resp.is_null() {
            return MFX_ERR_NULL_PTR;
        }
        let state = &mut *(pthis as *mut D3D11Allocator);
        let mids_ptr = (*resp).mids;
        if let Some(pos) = state.batches.iter().position(|b| b.mids.as_ptr() == mids_ptr) {
            state.batches.swap_remove(pos);
        }
        (*resp).mids = std::ptr::null_mut();
        (*resp).num_frame_actual = 0;
        MFX_ERR_NONE
    }
}

// ---- QsvEncoder ----

/// Fixed multiplier so `TargetKbps` (`u16`) covers up to 262 Mbps and never
/// needs to change across `MFXVideoENCODE_Reset` calls.
const BRC_MULT: u16 = 4;

/// Rate buffer, in frames at the starting bitrate. With one frame the encoder
/// spends a few percent of each frame's budget and never sharpens a still
/// picture past its key frame's quality: Arc HEVC, 2880x1920 text, 33 Mbps at
/// 120 fps (2026-11-24): 30 dB for good, so a picture stays blocky after every
/// key frame (connecting, another resolution) until its parts are redrawn.
/// Four frames: 39 dB within 1.3 s, scrolling 34 dB to 39, key frames the
/// same size, and the frames right after a bitrate cut smaller (at most 1.9
/// times their budget instead of 6), with no encoder errors on changing the
/// bitrate every frame.
const VBV_FRAMES: f64 = 4.0;

pub struct QsvEncoder {
    api: &'static VplApi,
    session: MfxSession,
    _alloc_state: Box<D3D11Allocator>,
    _alloc_table: Box<MfxFrameAllocator>,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    /// Lazily allocated 16-aligned NV12 GPU surface when caller passes an
    /// unaligned texture (e.g. 1920x1080 -> 1920x1088).
    aligned_tex: Option<ID3D11Texture2D>,
    input_mid: Box<MidEntry>,
    params: MfxVideoParam,
    _ext_co: Box<MfxExtCodingOption>,
    _ext_vsi: Box<MfxExtVideoSignalInfo>,
    _ext_co3: Box<MfxExtCodingOption3>,
    ext_ptrs: Box<[*mut MfxExtBuffer; 3]>,
    bs_buf: Vec<u8>,
    pub name: String,
    pub format: Codec,
    pub bitrate: u32,
    init_max_kbps: u16,
    width: u32,
    height: u32,
    aligned_w: u32,
    aligned_h: u32,
    fps: u32,
    index: u64,
    key_next: bool,
    rate_changed: bool,
    qp_control: bool,
    reset_failed_until: Option<Instant>,
}

fn create_aligned_tex(device: &ID3D11Device, aligned_w: u32, aligned_h: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: aligned_w,
        Height: aligned_h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_DECODER.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut t: Option<ID3D11Texture2D> = None;
    let mut r = unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)) };
    if r.is_err() {
        let mut d2 = desc;
        d2.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
        r = unsafe { device.CreateTexture2D(&d2, None, Some(&mut t)) };
    }
    r.context("CreateTexture2D(qsv aligned)")?;
    t.context("CreateTexture2D(qsv aligned)")
}

impl QsvEncoder {
    pub fn try_new(
        device: &ID3D11Device,
        luid: LUID,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        opts: EncOptions,
    ) -> Result<QsvEncoder> {
        if opts.yuv444 {
            bail!("QSV 4:4:4 not enabled");
        }
        let Some(adapter_idx) = intel_adapter_index(luid) else {
            bail!("not an Intel adapter");
        };
        let Some(api) = vpl_api() else {
            bail!("oneVPL dispatcher (libvpl.dll) not found");
        };
        let codec_id = match opts.codec {
            Codec::H264 => MFX_CODEC_AVC,
            Codec::Hevc => MFX_CODEC_HEVC,
        };
        unsafe {
            let init_session = |with_adapter_idx: bool| -> Result<MfxSession> {
                let adapter = with_adapter_idx.then_some(adapter_idx);
                let mut loaders = LOADERS.lock().unwrap();
                if let Some(i) = loaders.iter().position(|c| c.codec_id == codec_id && c.adapter == adapter) {
                    let kept = loaders[i].loader;
                    let mut session: MfxSession = std::ptr::null_mut();
                    let sts = (api.mfx_create_session)(kept, 0, &mut session);
                    if sts >= MFX_ERR_NONE && !session.is_null() {
                        return Ok(session);
                    }
                    crate::log!("MFXCreateSession from the kept loader failed: {sts}; loading again");
                    (api.mfx_unload)(kept);
                    loaders.remove(i);
                }
                let loader = (api.mfx_load)();
                if loader.is_null() {
                    bail!("MFXLoad returned null");
                }
                let setup = || -> Result<MfxSession> {
                    let set_u32_filter = |prop: &[u8], val: u32| -> Result<()> {
                        let cfg = (api.mfx_create_config)(loader);
                        if cfg.is_null() {
                            bail!("MFXCreateConfig failed");
                        }
                        let v = MfxVariant { version: 0, _pad: 0, ty: MFX_VARIANT_TYPE_U32, data: val as u64 };
                        let sts = (api.mfx_set_config_filter_property)(cfg, prop.as_ptr(), v);
                        if sts < MFX_ERR_NONE {
                            bail!("MFXSetConfigFilterProperty failed: {sts}");
                        }
                        Ok(())
                    };
                    set_u32_filter(b"mfxImplDescription.Impl\0", MFX_IMPL_TYPE_HARDWARE)?;
                    set_u32_filter(b"mfxImplDescription.AccelerationMode\0", MFX_ACCEL_MODE_VIA_D3D11)?;
                    set_u32_filter(b"mfxImplDescription.mfxEncoderDescription.encoder.CodecID\0", codec_id)?;
                    if with_adapter_idx {
                        set_u32_filter(b"DXGIAdapterIndex\0", adapter_idx)?;
                    }
                    let mut session: MfxSession = std::ptr::null_mut();
                    let sts = (api.mfx_create_session)(loader, 0, &mut session);
                    if sts < MFX_ERR_NONE || session.is_null() {
                        bail!("MFXCreateSession failed: {sts}");
                    }
                    Ok(session)
                };
                match setup() {
                    Ok(session) => {
                        loaders.push(CachedLoader { codec_id, adapter, loader });
                        Ok(session)
                    }
                    Err(e) => {
                        (api.mfx_unload)(loader);
                        Err(e)
                    }
                }
            };
            let t_start = Instant::now();
            let session = match init_session(true) {
                Ok(session) => session,
                Err(e) => {
                    crate::log!(
                        "oneVPL DXGIAdapterIndex={adapter_idx} filter unavailable ({e:#}); falling back to default adapter"
                    );
                    init_session(false)?
                }
            };

            let ctx = device.GetImmediateContext().context("GetImmediateContext")?;
            let mut alloc_state = Box::new(D3D11Allocator {
                device: device.clone(),
                ctx: ctx.clone(),
                batches: Vec::new(),
            });
            let mut alloc_table = Box::new(MfxFrameAllocator {
                reserved: [0; 4],
                pthis: (&mut *alloc_state as *mut D3D11Allocator) as MfxHdl,
                alloc: alloc_frames,
                lock: lock_frame,
                unlock: unlock_frame,
                get_hdl,
                free: free_frames,
            });

            let sts = (api.set_handle)(session, MFX_HANDLE_D3D11_DEVICE, device.as_raw());
            if sts < MFX_ERR_NONE {
                (api.mfx_close)(session);
                bail!("MFXVideoCORE_SetHandle(D3D11) failed: {sts}");
            }
            let sts = (api.set_frame_allocator)(session, &mut *alloc_table);
            if sts < MFX_ERR_NONE {
                (api.mfx_close)(session);
                bail!("MFXVideoCORE_SetFrameAllocator failed: {sts}");
            }

            let aligned_w = (width + 15) & !15;
            let aligned_h = (height + 15) & !15;
            let vbv_frames = opts.vbv_frames.unwrap_or(VBV_FRAMES);
            let target_usage = match opts.speed {
                None => {
                    if fps <= 60 {
                        MFX_TARGETUSAGE_BALANCED
                    } else {
                        MFX_TARGETUSAGE_BEST_SPEED
                    }
                }
                Some(s) => (7 - (s.min(100) * 6 / 100) as u16).clamp(MFX_TARGETUSAGE_BEST_QUALITY, MFX_TARGETUSAGE_BEST_SPEED),
            };

            let mut ext_co = Box::new(MfxExtCodingOption {
                header: MfxExtBuffer {
                    buffer_id: MFX_EXTBUFF_CODING_OPTION,
                    buffer_sz: size_of::<MfxExtCodingOption>() as u32,
                },
                max_dec_frame_buffering: 1,
                nal_hrd_conformance: MFX_CODINGOPTION_OFF,
                vui_nal_hrd_parameters: MFX_CODINGOPTION_OFF,
                pic_timing_sei: MFX_CODINGOPTION_OFF,
                au_delimiter: MFX_CODINGOPTION_OFF,
                ..Default::default()
            });

            let mut ext_vsi = Box::new(MfxExtVideoSignalInfo {
                header: MfxExtBuffer {
                    buffer_id: MFX_EXTBUFF_VIDEO_SIGNAL_INFO,
                    buffer_sz: size_of::<MfxExtVideoSignalInfo>() as u32,
                },
                video_format: 5,
                video_full_range: 0,
                colour_description_present: 1,
                colour_primaries: 1,
                transfer_characteristics: 1,
                matrix_coefficients: 1,
            });

            let mut ext_co3 = Box::new(MfxExtCodingOption3 {
                header: MfxExtBuffer {
                    buffer_id: MFX_EXTBUFF_CODING_OPTION3,
                    buffer_sz: size_of::<MfxExtCodingOption3>() as u32,
                },
                low_delay_brc: MFX_CODINGOPTION_ON,
                scenario_info: MFX_SCENARIO_DISPLAY_REMOTING,
                ..Default::default()
            });

            let mut ext_ptrs = Box::new([
                (&mut *ext_co as *mut MfxExtCodingOption) as *mut MfxExtBuffer,
                (&mut *ext_vsi as *mut MfxExtVideoSignalInfo) as *mut MfxExtBuffer,
                (&mut *ext_co3 as *mut MfxExtCodingOption3) as *mut MfxExtBuffer,
            ]);

            let mut params = MfxVideoParam {
                async_depth: 1,
                io_pattern: MFX_IOPATTERN_IN_VIDEO_MEMORY,
                mfx: MfxInfoMfx {
                    codec_id,
                    codec_profile: match opts.codec {
                        Codec::H264 => MFX_PROFILE_AVC_HIGH,
                        Codec::Hevc => MFX_PROFILE_HEVC_MAIN,
                    },
                    low_power: MFX_CODINGOPTION_ON,
                    target_usage,
                    gop_pic_size: (fps * 60).min(u16::MAX as u32) as u16,
                    gop_ref_dist: 1, // no B-frames
                    idr_interval: 0,
                    num_ref_frame: 1,
                    frame_info: MfxFrameInfo {
                        fourcc: MFX_FOURCC_NV12,
                        chroma_format: MFX_CHROMAFORMAT_YUV420,
                        pic_struct: MFX_PICSTRUCT_PROGRESSIVE,
                        width: aligned_w as u16,
                        height: aligned_h as u16,
                        crop_x: 0,
                        crop_y: 0,
                        crop_w: width as u16,
                        crop_h: height as u16,
                        frame_rate_ext_n: fps.max(1),
                        frame_rate_ext_d: 1,
                        aspect_ratio_w: 1,
                        aspect_ratio_h: 1,
                        bit_depth_luma: 8,
                        bit_depth_chroma: 8,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ext_param: ext_ptrs.as_mut_ptr(),
                num_ext_param: 3,
                ..Default::default()
            };
            apply_rate_params(&mut params, bitrate, fps, vbv_frames, opts.qp_control);

            // Try LowPower=ON (dedicated VDENC fixed-function hardware) first;
            // fall back to LowPower=OFF on older Gen iGPUs without VDENC, and
            // drop optional extension buffers if an older driver rejects them.
            let mut sts = MFX_ERR_UNSUPPORTED;
            'init: for num_ext in [3u16, 2, 1] {
                params.num_ext_param = num_ext;
                for lp in [MFX_CODINGOPTION_ON, MFX_CODINGOPTION_OFF] {
                    params.mfx.low_power = lp;
                    sts = (api.encode_init)(session, &mut params);
                    if sts >= MFX_ERR_NONE {
                        break 'init;
                    }
                }
            }
            if sts < MFX_ERR_NONE {
                (api.mfx_close)(session);
                bail!("MFXVideoENCODE_Init failed: {sts}");
            }
            // Populate `params` (and attached ext buffers) with the exact active
            // configuration accepted by QSV so future `MFXVideoENCODE_Reset`
            // calls don't fail with MFX_ERR_INCOMPATIBLE_VIDEO_PARAM (-14).
            let _ = (api.encode_get_video_param)(session, &mut params);

            let lp = if params.mfx.low_power == MFX_CODINGOPTION_ON { "VDENC" } else { "VME" };
            let name = format!("Intel QSV {} ({lp}, oneVPL)", opts.codec.name());
            crate::log!(
                "encoder {name}: {width}x{height} (aligned {aligned_w}x{aligned_h}) @ {fps} fps, TU={target_usage}, init status={sts}, ready in {} ms",
                t_start.elapsed().as_millis()
            );

            let init_max_kbps = params.mfx.max_kbps.max(params.mfx.target_kbps).max(1);
            let actual_bitrate = if !opts.qp_control && params.mfx.target_kbps > 0 {
                (params.mfx.target_kbps as u32) * (params.mfx.brc_param_multiplier.max(1) as u32) * 1000
            } else {
                bitrate
            };
            Ok(QsvEncoder {
                api,
                session,
                _alloc_state: alloc_state,
                _alloc_table: alloc_table,
                device: device.clone(),
                ctx,
                aligned_tex: None,
                input_mid: Box::new(MidEntry { res: AllocResource::External(std::ptr::null_mut()), is_p8: false }),
                params,
                _ext_co: ext_co,
                _ext_vsi: ext_vsi,
                _ext_co3: ext_co3,
                ext_ptrs,
                bs_buf: vec![0u8; (width as usize * height as usize).max(1024 * 1024)],
                name,
                format: opts.codec,
                bitrate: actual_bitrate,
                init_max_kbps,
                width,
                height,
                aligned_w,
                aligned_h,
                fps,
                index: 0,
                key_next: true,
                rate_changed: false,
                qp_control: opts.qp_control,
                reset_failed_until: None,
            })
        }
    }

    pub fn aligned_size(&self) -> (u32, u32) {
        (self.aligned_w, self.aligned_h)
    }

    pub fn set_bitrate(&mut self, bps: u32) {
        if self.qp_control {
            return;
        }
        if self.reset_failed_until.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let prev = self.params;
        // Keep buffer_size_in_kb & initial_delay_in_kb fixed across Reset and
        // stay within the initial HRD ceiling (`init_max_kbps`): QSV adjusts
        // TargetKbps/MaxKbps in place without a keyframe when both hold.
        let mult = self.params.mfx.brc_param_multiplier.max(1) as u32;
        let kbps = (bps / 1000).max(100);
        let scaled = (kbps / mult).clamp(1, self.init_max_kbps as u32) as u16;
        if scaled == self.params.mfx.target_kbps && scaled == self.params.mfx.max_kbps {
            return;
        }
        self.params.mfx.target_kbps = scaled;
        self.params.mfx.max_kbps = scaled;
        self.params.ext_param = self.ext_ptrs.as_mut_ptr();
        let sts = unsafe { (self.api.encode_reset)(self.session, &mut self.params) };
        if sts >= MFX_ERR_NONE {
            let actual_mult = self.params.mfx.brc_param_multiplier.max(1) as u32;
            self.bitrate = (scaled as u32) * actual_mult * 1000;
            self.rate_changed = true;
            self.reset_failed_until = None;
        } else {
            crate::log!(
                "QSV set_bitrate({bps}) reset failed: {sts} (mult={mult}, buf={}, init_delay={})",
                self.params.mfx.buffer_size_in_kb,
                self.params.mfx.initial_delay_in_kb
            );
            self.params = prev;
            self.reset_failed_until = Some(Instant::now() + Duration::from_secs(3));
        }
    }

    pub fn request_key(&mut self) {
        self.key_next = true;
    }

    pub fn encode(
        &mut self,
        nv12: &ID3D11Texture2D,
        timeout: Duration,
        qp: Option<u32>,
        rekeys_on_rate_change: &mut bool,
    ) -> Result<Option<Encoded>> {
        let mut tex_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { nv12.GetDesc(&mut tex_desc) };
        let src_ptr = if tex_desc.Width == self.aligned_w && tex_desc.Height == self.aligned_h {
            nv12.as_raw()
        } else {
            if self.aligned_tex.is_none() {
                self.aligned_tex = Some(create_aligned_tex(&self.device, self.aligned_w, self.aligned_h)?);
            }
            let aligned = self.aligned_tex.as_ref().expect("created above");
            let copy_w = tex_desc.Width.min(self.width);
            let copy_h = tex_desc.Height.min(self.height);
            unsafe {
                let bx = D3D11_BOX { left: 0, top: 0, front: 0, right: copy_w, bottom: copy_h, back: 1 };
                self.ctx.CopySubresourceRegion(aligned, 0, 0, 0, 0, nv12, 0, Some(&bx));
            }
            aligned.as_raw()
        };
        self.input_mid.res = AllocResource::External(src_ptr);

        let mut surf = MfxFrameSurface1 {
            info: self.params.mfx.frame_info,
            data: MfxFrameData {
                mem_id: (&mut *self.input_mid as *mut MidEntry) as MfxMemId,
                mem_type: MFX_MEMTYPE_VIDEO_MEMORY_DECODER_TARGET | MFX_MEMTYPE_EXTERNAL_FRAME,
                time_stamp: self.index * (90_000 / self.fps.max(1) as u64),
                ..Default::default()
            },
            ..Default::default()
        };

        let asked_key = std::mem::take(&mut self.key_next);
        let mut ctrl = MfxEncodeCtrl {
            frame_type: if asked_key { MFX_FRAMETYPE_I | MFX_FRAMETYPE_REF | MFX_FRAMETYPE_IDR } else { 0 },
            qp: qp.map_or(0, |q| q.min(51) as u16),
            ..Default::default()
        };
        let ctrl_ptr = if asked_key || qp.is_some() { &mut ctrl as *mut MfxEncodeCtrl } else { std::ptr::null_mut() };

        let deadline = Instant::now() + timeout;
        let mut buf_retries = 0u32;

        loop {
            let mut bs = MfxBitstream {
                data: self.bs_buf.as_mut_ptr(),
                max_length: self.bs_buf.len() as u32,
                ..Default::default()
            };
            let mut syncp: MfxSyncPoint = std::ptr::null_mut();
            let sts = unsafe { (self.api.encode_frame_async)(self.session, ctrl_ptr, &mut surf, &mut bs, &mut syncp) };
            match sts {
                s if s == MFX_ERR_NOT_ENOUGH_BUFFER => {
                    buf_retries += 1;
                    const MAX_BS_BUF: usize = 64 * 1024 * 1024;
                    if buf_retries > 6 || self.bs_buf.len() >= MAX_BS_BUF {
                        if asked_key {
                            self.key_next = true;
                        }
                        bail!("QSV bitstream buffer limit exceeded ({} bytes, retries={buf_retries})", self.bs_buf.len());
                    }
                    let new_len = self.bs_buf.len().saturating_mul(2).clamp(4 * 1024 * 1024, MAX_BS_BUF);
                    self.bs_buf.resize(new_len, 0);
                    continue;
                }
                s if s == MFX_WRN_DEVICE_BUSY => {
                    if Instant::now() >= deadline {
                        if asked_key {
                            self.key_next = true;
                        }
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_micros(200));
                    continue;
                }
                s if s == MFX_ERR_MORE_DATA => {
                    self.index += 1;
                    return Ok(None);
                }
                s if s < MFX_ERR_NONE => bail!("MFXVideoENCODE_EncodeFrameAsync failed: {s}"),
                _ => {}
            }
            self.index += 1;
            if syncp.is_null() {
                return Ok(None);
            }
            let sync_deadline = Instant::now() + timeout;
            loop {
                let rem_ms = sync_deadline.saturating_duration_since(Instant::now()).as_millis().clamp(1, 60_000) as u32;
                let ssts = unsafe { (self.api.sync_operation)(self.session, syncp, rem_ms) };
                if ssts == MFX_WRN_IN_EXECUTION {
                    if Instant::now() >= sync_deadline {
                        return Err(anyhow!("qsv sync timeout"));
                    }
                    continue;
                }
                if ssts < MFX_ERR_NONE {
                    bail!("MFXVideoCORE_SyncOperation failed: {ssts}");
                }
                break;
            }
            let start = bs.data_offset as usize;
            let len = bs.data_length as usize;
            let end = (start + len).min(self.bs_buf.len());
            let mut data = self.bs_buf[start..end].to_vec();
            let padding = data.iter().rev().take_while(|&&b| b == 0).count();
            data.truncate(data.len() - padding);
            let key = (bs.frame_type & (MFX_FRAMETYPE_IDR | MFX_FRAMETYPE_I)) != 0;
            let after_change = std::mem::take(&mut self.rate_changed);
            if key && !asked_key && after_change && !*rekeys_on_rate_change {
                *rekeys_on_rate_change = true;
                crate::log!("encoder starts a key frame on every bitrate change; changing the bitrate rarely from now on");
            }
            return Ok(Some(Encoded { data, key, qp, padding }));
        }
    }
}

fn apply_rate_params(params: &mut MfxVideoParam, bitrate_bps: u32, fps: u32, vbv_frames: f64, qp_control: bool) {
    if qp_control {
        params.mfx.rate_control_method = MFX_RATECONTROL_CQP;
        params.mfx.brc_param_multiplier = 1;
        params.mfx.initial_delay_in_kb = 26; // QPI
        params.mfx.target_kbps = 26; // QPP
        params.mfx.max_kbps = 26; // QPB
        params.mfx.buffer_size_in_kb = 0;
    } else {
        let kbps = (bitrate_bps / 1000).max(100);
        let mult = BRC_MULT as u32;
        let scaled_kbps = (kbps / mult).clamp(1, u16::MAX as u32) as u16;
        let buf_kb = ((bitrate_bps as f64 / 8.0 / fps.max(1) as f64 * vbv_frames) / 1000.0).ceil().max(8.0) as u32;
        let scaled_buf = (buf_kb / mult).clamp(2, u16::MAX as u32) as u16;
        params.mfx.rate_control_method = MFX_RATECONTROL_CBR;
        params.mfx.brc_param_multiplier = BRC_MULT;
        params.mfx.target_kbps = scaled_kbps;
        params.mfx.max_kbps = scaled_kbps;
        params.mfx.buffer_size_in_kb = scaled_buf;
        params.mfx.initial_delay_in_kb = (scaled_buf / 2).max(1);
    }
}

impl Drop for QsvEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.api.encode_close)(self.session);
            let _ = (self.api.mfx_close)(self.session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_params_scale_high_bitrates_without_u16_overflow() {
        let mut p = MfxVideoParam::default();
        apply_rate_params(&mut p, 80_000_000, 120, 1.0, false);
        assert_eq!(p.mfx.rate_control_method, MFX_RATECONTROL_CBR);
        assert_eq!(p.mfx.brc_param_multiplier, BRC_MULT);
        assert_eq!(p.mfx.target_kbps as u32 * p.mfx.brc_param_multiplier as u32, 80_000);
        assert_eq!(p.mfx.max_kbps, p.mfx.target_kbps);
        assert!(p.mfx.buffer_size_in_kb >= 2);
        assert!(p.mfx.initial_delay_in_kb >= 1);

        apply_rate_params(&mut p, 150_000_000, 120, 1.0, false);
        assert_eq!(p.mfx.target_kbps as u32 * p.mfx.brc_param_multiplier as u32, 150_000);
    }
}
