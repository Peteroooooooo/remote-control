//! Display HDR state detection (Windows DisplayConfig) and scRGB -> SDR sRGB
//! tone mapping shared between the Rust reference path and D3D11 HLSL shaders.

use std::mem::size_of;

use windows::Win32::{
    Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
        DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
        QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
    },
    Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS},
};

/// HLSL helper matching [`hdr_scrgb_to_srgb`] 1-for-1 for D3D11 shaders.
///
/// Luminance `l <= 0.85` is passed through unmodified (zero bias across
/// standard SDR UI and dark/mid-tone content); highlights above `0.85` roll
/// off smoothly with a C1 rational shoulder (`K = 0.85, H = 0.15`) before
/// applying the exact sRGB OETF.
#[allow(dead_code)]
pub const HLSL_HDR_TO_SRGB: &str = r#"
float3 hdr_to_srgb(float3 c, float inv_sdr_white) {
    c = max(c, 0.0) * inv_sdr_white;
    float l = dot(c, float3(0.2126, 0.7152, 0.0722));
    if (l > 0.85) {
        float over = l - 0.85;
        float l_new = 0.85 + 0.15 * (over / (over + 0.15));
        c *= (l_new / l);
    }
    c = saturate(c);
    float3 lo = c * 12.92;
    float3 hi = 1.055 * pow(max(c, 0.0031308), 1.0 / 2.4) - 0.055;
    return lerp(lo, hi, step(0.0031308, c));
}
"#;

/// Per-display HDR state and SDR white-level scale in scRGB units (`1.0 = 80 nits`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayHdrInfo {
    /// Whether Windows Advanced Color / HDR is enabled on the display.
    pub hdr_enabled: bool,
    /// Windows "SDR content brightness" slider in scRGB units (`1.0 = 80 nits`,
    /// e.g. `2.5` for 200 nits). Defaults to `1.0` when SDR or unavailable.
    pub sdr_white_scale: f32,
}

impl DisplayHdrInfo {
    /// Default SDR display description (`hdr_enabled = false`, `sdr_white_scale = 1.0`).
    pub fn sdr() -> Self {
        Self {
            hdr_enabled: false,
            sdr_white_scale: 1.0,
        }
    }

    /// Reciprocal of `sdr_white_scale`, clamped safely to `[40 nits, 1000 nits]`
    /// (`[0.5, 12.5]` in scRGB units) so corrupt driver values cannot divide by
    /// zero or blow up.
    pub fn inv_sdr_white(&self) -> f32 {
        let clamped = if self.sdr_white_scale.is_finite() {
            self.sdr_white_scale.clamp(0.5, 12.5)
        } else {
            1.0
        };
        1.0 / clamped
    }
}

/// Query Windows DisplayConfig for the HDR status and SDR white level of the
/// active display matching `device_name` (e.g. `\\.\DISPLAY1`).
///
/// Never panics; returns [`DisplayHdrInfo::sdr()`] if any Win32 call fails or
/// no active path matches `device_name`.
pub fn query_display_hdr(device_name: &str) -> DisplayHdrInfo {
    let mut paths = Vec::<DISPLAYCONFIG_PATH_INFO>::new();
    let mut modes = Vec::<DISPLAYCONFIG_MODE_INFO>::new();

    // Retry briefly if topology changes between GetDisplayConfigBufferSizes and QueryDisplayConfig.
    let mut queried = false;
    for _ in 0..4 {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        let err = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        };
        if err != ERROR_SUCCESS || path_count == 0 {
            return DisplayHdrInfo::sdr();
        }
        paths.resize(path_count as usize, DISPLAYCONFIG_PATH_INFO::default());
        modes.resize(mode_count as usize, DISPLAYCONFIG_MODE_INFO::default());
        let err = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if err == ERROR_SUCCESS {
            paths.truncate(path_count as usize);
            queried = true;
            break;
        }
        if err != ERROR_INSUFFICIENT_BUFFER {
            return DisplayHdrInfo::sdr();
        }
    }
    if !queried {
        return DisplayHdrInfo::sdr();
    }

    for path in &paths {
        let mut src_name = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        src_name.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut src_name.header) } != ERROR_SUCCESS.0 as i32 {
            continue;
        }
        let len = src_name
            .viewGdiDeviceName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(src_name.viewGdiDeviceName.len());
        let gdi_name = String::from_utf16_lossy(&src_name.viewGdiDeviceName[..len]);
        if !gdi_name.eq_ignore_ascii_case(device_name) {
            continue;
        }

        let mut adv = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO::default();
        adv.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
            size: size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut adv.header) } != ERROR_SUCCESS.0 as i32 {
            return DisplayHdrInfo::sdr();
        }
        // Bit 1 of DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO_0_0 is `advancedColorEnabled`.
        let hdr_enabled = unsafe { (adv.Anonymous.value & 0x2) != 0 };

        let mut sdr_white = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
        sdr_white.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
            size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        };
        let sdr_white_scale = if unsafe { DisplayConfigGetDeviceInfo(&mut sdr_white.header) }
            != ERROR_SUCCESS.0 as i32
            || sdr_white.SDRWhiteLevel == 0
        {
            1.0
        } else {
            (sdr_white.SDRWhiteLevel as f32 / 1000.0).clamp(0.5, 12.5)
        };

        return DisplayHdrInfo {
            hdr_enabled,
            sdr_white_scale,
        };
    }

    DisplayHdrInfo::sdr()
}

/// Pure Rust reference tone-mapping function matching [`HLSL_HDR_TO_SRGB`] 1-for-1.
///
/// Maps linear scRGB `rgb` normalized by `inv_sdr_white` to display-referred
/// sRGB in `[0.0, 1.0]`.
#[cfg(test)]
pub fn hdr_scrgb_to_srgb(rgb: [f32; 3], inv_sdr_white: f32) -> [f32; 3] {
    const K: f32 = 0.85;
    const H: f32 = 0.15;

    let inv = if inv_sdr_white.is_finite() {
        inv_sdr_white.max(0.0)
    } else {
        0.0
    };

    let mut x = [
        if rgb[0].is_finite() {
            (rgb[0].max(0.0) * inv).min(1.0e6)
        } else {
            0.0
        },
        if rgb[1].is_finite() {
            (rgb[1].max(0.0) * inv).min(1.0e6)
        } else {
            0.0
        },
        if rgb[2].is_finite() {
            (rgb[2].max(0.0) * inv).min(1.0e6)
        } else {
            0.0
        },
    ];

    let l = 0.2126 * x[0] + 0.7152 * x[1] + 0.0722 * x[2];
    if l > K {
        let over = l - K;
        let l_new = K + H * (over / (over + H));
        let scale = l_new / l;
        x[0] *= scale;
        x[1] *= scale;
        x[2] *= scale;
    }

    x.map(|v| {
        let c = v.clamp(0.0, 1.0);
        if c <= 0.0031308 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb_to_linear(s: f32) -> f32 {
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }

    #[test]
    fn pure_black() {
        assert_eq!(hdr_scrgb_to_srgb([0.0, 0.0, 0.0], 1.0), [0.0, 0.0, 0.0]);
        assert_eq!(hdr_scrgb_to_srgb([0.0, 0.0, 0.0], 0.4), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn sdr_mid_gray_exact_srgb_oetf() {
        let sdr_white = 2.5;
        let inv = 1.0 / sdr_white;
        let out = hdr_scrgb_to_srgb([0.5 * sdr_white; 3], inv);
        let expected = 1.055 * 0.5f32.powf(1.0 / 2.4) - 0.055;
        for ch in out {
            assert!((ch - expected).abs() < 1e-6, "got {ch}, expected {expected}");
        }
    }

    #[test]
    fn sdr_white_bright_and_smoothly_compressed() {
        let sdr_white = 2.5;
        let inv = 1.0 / sdr_white;
        let out = hdr_scrgb_to_srgb([1.0 * sdr_white; 3], inv);
        // l = 1.0 > 0.85 -> over = 0.15 -> l_new = 0.85 + 0.15 * 0.5 = 0.925
        let expected = 1.055 * 0.925f32.powf(1.0 / 2.4) - 0.055;
        for ch in out {
            assert!((ch - expected).abs() < 1e-6, "got {ch}, expected {expected}");
            assert!(ch > 0.96 && ch < 0.97);
        }
    }

    #[test]
    fn monotonic_across_hdr_range() {
        let levels = [0.0f32, 0.1, 0.5, 0.85, 1.0, 2.0, 5.0, 10.0];
        let mut prev = -1.0f32;
        for &lv in &levels {
            let out = hdr_scrgb_to_srgb([lv, lv, lv], 1.0);
            assert!(out[0] > prev, "not strictly monotonic at {lv}: {} <= {prev}", out[0]);
            assert!(out[0] <= 1.0);
            prev = out[0];
        }
    }

    #[test]
    fn preserves_hue_when_channels_stay_in_gamut() {
        // Luminance = 0.2126*0.95 + 0.7152*0.88 + 0.0722*0.75 = 0.885496 > 0.85,
        // and all scaled channels stay <= 1.0 before sRGB OETF.
        let rgb = [0.95f32, 0.88, 0.75];
        let out = hdr_scrgb_to_srgb(rgb, 1.0);
        let lin = out.map(srgb_to_linear);
        let ratio_rg_in = rgb[0] / rgb[1];
        let ratio_rg_out = lin[0] / lin[1];
        let ratio_bg_in = rgb[2] / rgb[1];
        let ratio_bg_out = lin[2] / lin[1];
        assert!((ratio_rg_in - ratio_rg_out).abs() < 1e-5);
        assert!((ratio_bg_in - ratio_bg_out).abs() < 1e-5);
    }

    #[test]
    fn clamps_negative_nan_and_inf_safely() {
        let out = hdr_scrgb_to_srgb([-1.0, f32::NAN, f32::INFINITY], 1.0);
        assert_eq!(out, [0.0, 0.0, 0.0]);

        let out2 = hdr_scrgb_to_srgb([f32::NEG_INFINITY, -0.5, 0.5], 1.0);
        assert_eq!(out2[0], 0.0);
        assert_eq!(out2[1], 0.0);
        let expected_b = 1.055 * 0.5f32.powf(1.0 / 2.4) - 0.055;
        assert!((out2[2] - expected_b).abs() < 1e-6);

        let out3 = hdr_scrgb_to_srgb([f32::MAX, 1.0, 0.0], 2.0);
        for ch in out3 {
            assert!(ch.is_finite() && (0.0..=1.0).contains(&ch));
        }

        let out4 = hdr_scrgb_to_srgb([1.0, 1.0, 1.0], f32::NAN);
        assert_eq!(out4, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn inv_sdr_white_clamps_to_safe_bounds() {
        let cases = [
            (0.0f32, 2.0f32),
            (0.1, 2.0),
            (2.5, 0.4),
            (999.0, 1.0 / 12.5),
            (f32::NAN, 1.0),
        ];
        for (scale, expected) in cases {
            let info = DisplayHdrInfo {
                hdr_enabled: true,
                sdr_white_scale: scale,
            };
            assert!(
                (info.inv_sdr_white() - expected).abs() < 1e-6,
                "scale={scale}: got {}, expected {expected}",
                info.inv_sdr_white()
            );
        }
    }
}
