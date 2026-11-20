//! Wi-Fi in media streaming mode while a picture streams: the adapter stops
//! scanning for other networks in the background and dozing between
//! packets, which otherwise holds packets back for tens of milliseconds now
//! and then (Moonlight does the same, `enterLowLatencyMode`). Windows keeps
//! the mode only while our WLAN handle is open.

use std::ffi::c_void;

use windows::{
    Win32::{
        Foundation::HANDLE,
        NetworkManagement::WiFi::{
            WLAN_INTERFACE_INFO_LIST, WlanCloseHandle, WlanEnumInterfaces, WlanFreeMemory, WlanOpenHandle,
            WlanSetInterface, wlan_interface_state_connected, wlan_intf_opcode_media_streaming_mode,
        },
    },
    core::GUID,
};

/// Guard that keeps Wi-Fi media streaming mode enabled for connected adapters
/// until dropped.
pub(crate) struct LowLatency {
    handle: HANDLE,
    ifaces: Vec<GUID>,
}

impl LowLatency {
    /// Enables media streaming mode on all currently connected Wi-Fi adapters.
    /// Returns `None` quietly if the WLAN service is unavailable or no adapter
    /// succeeded.
    pub(crate) fn enter() -> Option<Self> {
        let mut negotiated = 0u32;
        let mut handle = HANDLE::default();
        let err = unsafe { WlanOpenHandle(2, None, &mut negotiated, &mut handle) };
        if err != 0 {
            return None;
        }

        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        let err = unsafe { WlanEnumInterfaces(handle, None, &mut list) };
        if err != 0 || list.is_null() {
            if !list.is_null() {
                unsafe { WlanFreeMemory(list.cast()) };
            }
            unsafe { WlanCloseHandle(handle, None) };
            return None;
        }

        let mut ifaces = Vec::new();
        let enable: i32 = 1;
        let items = unsafe {
            std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), (*list).dwNumberOfItems as usize)
        };
        for iface in items {
            if iface.isState != wlan_interface_state_connected {
                continue;
            }
            let len = iface
                .strInterfaceDescription
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(iface.strInterfaceDescription.len());
            let desc = String::from_utf16_lossy(&iface.strInterfaceDescription[..len]);
            let rc = unsafe {
                WlanSetInterface(
                    handle,
                    &iface.InterfaceGuid,
                    wlan_intf_opcode_media_streaming_mode,
                    std::mem::size_of::<i32>() as u32,
                    (&enable as *const i32).cast::<c_void>(),
                    None,
                )
            };
            if rc == 0 {
                crate::log!("Wi-Fi {desc}: low-latency (media streaming) mode on");
                ifaces.push(iface.InterfaceGuid);
            } else {
                crate::log!("Wi-Fi {desc}: media streaming mode failed (err {rc})");
            }
        }

        unsafe { WlanFreeMemory(list.cast()) };

        if ifaces.is_empty() {
            unsafe { WlanCloseHandle(handle, None) };
            return None;
        }

        Some(Self { handle, ifaces })
    }
}

impl Drop for LowLatency {
    fn drop(&mut self) {
        let disable: i32 = 0;
        for guid in &self.ifaces {
            unsafe {
                let _ = WlanSetInterface(
                    self.handle,
                    guid,
                    wlan_intf_opcode_media_streaming_mode,
                    std::mem::size_of::<i32>() as u32,
                    (&disable as *const i32).cast::<c_void>(),
                    None,
                );
            }
        }
        unsafe {
            let _ = WlanCloseHandle(self.handle, None);
        }
    }
}
