//! WASAPI process/endpoint loopback capture, host speaker mute guard, and viewer audio playback.

use std::{
    collections::{BTreeMap, VecDeque},
    mem::ManuallyDrop,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use iroh::endpoint::RecvStream;
use rc_stream::wire::{T_AUDIO, T_AUDIO_PCM24};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, error::TryRecvError};
use windows::{
    Win32::{
        Foundation::{E_NOINTERFACE, E_POINTER, RPC_E_CHANGED_MODE, S_FALSE, S_OK},
        Media::{
            Audio::{
                AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
                AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
                AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
                Endpoints::IAudioEndpointVolume, IActivateAudioInterfaceAsyncOperation,
                IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
                IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
                MMDeviceEnumerator, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVE_FORMAT_PCM, WAVEFORMATEX,
                WAVEFORMATEXTENSIBLE, eConsole, eRender,
            },
            KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE},
            Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT},
        },
        System::{
            Com::{
                BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                CoUninitialize, IAgileObject, IAgileObject_Impl,
                StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0},
            },
            Variant::VT_BLOB,
        },
    },
    core::{HRESULT, HSTRING, Interface},
};

use super::codec::{
    CODEC_OPUS, CODEC_PCM24, Decoder, DownmixResampler, Encoder, FRAME_SAMPLES, MAX_PACKET_BYTES, PcmFormat,
    SAMPLE_RATE, SampleFmt, SilenceGate, decode_pcm24, encode_pcm24, normalize_lossless_rate,
};
use crate::log;

/// Maximum lossless audio frame payload size accepted over the uni-stream (64 KB).
const MAX_LOSSLESS_FRAME: usize = 65_536;

pub struct ComGuard {
    uninit: bool,
}

impl ComGuard {
    pub fn init() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == S_OK || hr == S_FALSE {
            Ok(Self { uninit: true })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self { uninit: false })
        } else {
            Err(windows::core::Error::from(hr).into())
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            unsafe { CoUninitialize() };
        }
    }
}

fn stereo_f32_wfx(rate: u32) -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: 2,
        nSamplesPerSec: rate,
        nAvgBytesPerSec: rate * 2 * 4,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

fn device_id(dev: &IMMDevice) -> Result<String> {
    unsafe {
        let pwstr = dev.GetId()?;
        let s = pwstr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(pwstr.0 as *const _));
        Ok(s)
    }
}

fn default_render_device(enumerator: &IMMDeviceEnumerator) -> Result<(IMMDevice, String)> {
    let dev = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }?;
    let id = device_id(&dev)?;
    Ok((dev, id))
}

fn default_render_rate(enumerator: &IMMDeviceEnumerator) -> Option<(String, u32)> {
    let (dev, id) = default_render_device(enumerator).ok()?;
    let ac: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None) }.ok()?;
    let mix_ptr = unsafe { ac.GetMixFormat() }.ok()?;
    let rate = unsafe { (*mix_ptr).nSamplesPerSec };
    unsafe { CoTaskMemFree(Some(mix_ptr as *const _)) };
    Some((id, normalize_lossless_rate(rate)))
}

/// Probes the local default render device's native sample rate (0 if unavailable).
pub fn local_render_rate() -> u32 {
    let Ok(_com) = ComGuard::init() else { return 0 };
    let Ok(enumerator) = (unsafe { CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) }) else {
        return 0;
    };
    default_render_rate(&enumerator).map(|(_, r)| r).unwrap_or(0)
}

fn effective_lossless_rate(host_rate: u32, viewer_max_rate: u32) -> u32 {
    let h = normalize_lossless_rate(host_rate);
    if viewer_max_rate == 0 {
        h
    } else {
        h.min(normalize_lossless_rate(viewer_max_rate))
    }
}

fn parse_mix_format(wfx_ptr: *const WAVEFORMATEX) -> Result<(PcmFormat, usize)> {
    let wfx = unsafe { ptr::read_unaligned(wfx_ptr) };
    let tag = wfx.wFormatTag as u32;
    let channels = wfx.nChannels;
    let rate = wfx.nSamplesPerSec;
    let bits = wfx.wBitsPerSample;
    let block_align = wfx.nBlockAlign as usize;

    let sample = if tag == WAVE_FORMAT_IEEE_FLOAT && bits == 32 {
        SampleFmt::F32
    } else if tag == WAVE_FORMAT_PCM && bits == 16 {
        SampleFmt::I16
    } else if tag == WAVE_FORMAT_PCM && bits == 24 {
        SampleFmt::I24
    } else if tag == WAVE_FORMAT_PCM && bits == 32 {
        SampleFmt::I32
    } else if tag == WAVE_FORMAT_EXTENSIBLE && wfx.cbSize >= 22 {
        let wfxe = unsafe { ptr::read_unaligned(wfx_ptr as *const WAVEFORMATEXTENSIBLE) };
        let sub = wfxe.SubFormat;
        if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
            SampleFmt::F32
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 16 {
            SampleFmt::I16
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 24 {
            SampleFmt::I24
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 32 {
            SampleFmt::I32
        } else {
            bail!("unsupported extensible WASAPI format: bits={bits}");
        }
    } else {
        bail!("unsupported WASAPI format: tag={tag} bits={bits}");
    };

    if channels == 0 || rate == 0 || block_align == 0 {
        bail!("invalid WASAPI mix format: rate={rate} ch={channels} align={block_align}");
    }
    Ok((PcmFormat { rate, channels, sample }, block_align))
}

#[windows_core::implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct ActivationHandler {
    tx: Mutex<Option<std::sync::mpsc::Sender<windows::core::Result<IAudioClient>>>>,
}

impl IAgileObject_Impl for ActivationHandler_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        op: windows_core::Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let res = (|| -> windows::core::Result<IAudioClient> {
            let op = op.as_ref().ok_or(windows::core::Error::from(E_POINTER))?;
            let mut hr = HRESULT(0);
            let mut iface = None;
            unsafe { op.GetActivateResult(&mut hr, &mut iface)? };
            hr.ok()?;
            let unk = iface.ok_or(windows::core::Error::from(E_NOINTERFACE))?;
            unk.cast::<IAudioClient>()
        })();
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(res);
        }
        Ok(())
    }
}

/// Opens a Windows 10 2004+ / Windows 11 Process Loopback client that captures all
/// application audio except our own process tree, bypassing endpoint-level speaker APOs
/// (Huawei Histen, Dolby, Nahimic, Waves, etc.).
fn open_process_loopback(target_rate: u32) -> Result<(IAudioClient, PcmFormat, usize)> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: std::process::id(),
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // Wrap in ManuallyDrop so PROPVARIANT's Drop does not call PropVariantClear /
    // CoTaskMemFree on our stack-allocated `params`.
    let prop = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&mut params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast::<u8>(),
                    },
                },
            }),
        },
    });

    let (tx, rx) = std::sync::mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler =
        ActivationHandler { tx: Mutex::new(Some(tx)) }.into();
    let _op = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*prop as *const PROPVARIANT),
            &handler,
        )
    }
    .context("ActivateAudioInterfaceAsync process loopback")?;

    let audio_client = rx
        .recv_timeout(Duration::from_secs(2))
        .context("process loopback activation timed out")?
        .context("process loopback activation failed")?;

    let wfx = stereo_f32_wfx(target_rate);
    let auto_flags =
        AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    let init_ok = unsafe {
        audio_client.Initialize(AUDCLNT_SHAREMODE_SHARED, auto_flags, 1_000_000, 0, &wfx, None)
    }
    .or_else(|_| unsafe {
        audio_client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 1_000_000, 0, &wfx, None)
    });
    init_ok.context("IAudioClient::Initialize process loopback")?;

    Ok((audio_client, PcmFormat { rate: target_rate, channels: 2, sample: SampleFmt::F32 }, 8))
}

fn open_endpoint_loopback(
    enumerator: &IMMDeviceEnumerator,
    target_rate: u32,
) -> Result<(IAudioClient, PcmFormat, usize, String)> {
    let (dev, cur_id) = default_render_device(enumerator).context("default render device")?;
    let audio_client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None) }.context("Activate IAudioClient")?;
    let wfx = stereo_f32_wfx(target_rate);
    let auto_loopback =
        AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    let (fmt, block_align) = if unsafe {
        audio_client.Initialize(AUDCLNT_SHAREMODE_SHARED, auto_loopback, 1_000_000, 0, &wfx, None)
    }
    .is_ok()
    {
        (PcmFormat { rate: target_rate, channels: 2, sample: SampleFmt::F32 }, 8)
    } else {
        let mix_ptr = unsafe { audio_client.GetMixFormat() }.context("GetMixFormat")?;
        let fmt_res = parse_mix_format(mix_ptr);
        let init_res = fmt_res.and_then(|(fmt, block_align)| {
            unsafe {
                audio_client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    1_000_000,
                    0,
                    mix_ptr,
                    None,
                )
            }
            .context("IAudioClient::Initialize loopback")?;
            Ok((fmt, block_align))
        });
        unsafe { CoTaskMemFree(Some(mix_ptr as *const _)) };
        init_res?
    };
    Ok((audio_client, fmt, block_align, cur_id))
}

/// Temporarily mutes the host's default render endpoint and always restores its
/// original mute state when disabled, switched, or dropped.
pub struct HostMuteGuard {
    enumerator: IMMDeviceEnumerator,
    saved: Option<(String, bool)>,
    want_muted: bool,
}

impl HostMuteGuard {
    pub fn new(enumerator: IMMDeviceEnumerator) -> Self {
        Self { enumerator, saved: None, want_muted: false }
    }

    pub fn set_muted(&mut self, mute: bool) {
        self.want_muted = mute;
        if !mute {
            self.restore();
            return;
        }
        let Ok((dev, id)) = default_render_device(&self.enumerator) else {
            return;
        };
        if let Some((prev_id, _)) = &self.saved {
            if *prev_id == id {
                return;
            }
            self.restore();
        }
        let Ok(vol) = (unsafe { dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }) else {
            return;
        };
        let orig = unsafe { vol.GetMute() }.map(|b| b.as_bool()).unwrap_or(false);
        if !orig && unsafe { vol.SetMute(true, ptr::null()) }.is_err() {
            return;
        }
        self.saved = Some((id, orig));
    }

    pub fn poll(&mut self) {
        if self.want_muted {
            self.set_muted(true);
        }
    }

    pub fn restore(&mut self) {
        let Some((id, orig_muted)) = self.saved.take() else {
            return;
        };
        if orig_muted {
            return;
        }
        let dev = unsafe { self.enumerator.GetDevice(&HSTRING::from(&id)) }
            .or_else(|_| unsafe { self.enumerator.GetDefaultAudioEndpoint(eRender, eConsole) });
        if let Ok(dev) = dev
            && let Ok(vol) = unsafe { dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }
        {
            let _ = unsafe { vol.SetMute(false, ptr::null()) };
        }
    }
}

impl Drop for HostMuteGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Best-effort unmute of the default render endpoint when an `audio-agent`
/// child exits abnormally or is killed before `HostMuteGuard::drop` runs.
pub fn force_unmute_default_render() {
    let Ok(_com) = ComGuard::init() else { return };
    let Ok(enumerator) = (unsafe { CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) }) else {
        return;
    };
    if let Ok((dev, _)) = default_render_device(&enumerator)
        && let Ok(vol) = (unsafe { dev.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) })
    {
        let _ = unsafe { vol.SetMute(false, ptr::null()) };
    }
}

pub struct CaptureState {
    pub stop: AtomicBool,
    pub enabled: AtomicBool,
    pub channels: AtomicU8,
    pub mute_host: AtomicBool,
    pub lossless: AtomicBool,
    pub max_rate_hz: AtomicU32,
}

impl CaptureState {
    pub fn new(enabled: bool, channels: u8, mute_host: bool, lossless: bool) -> Self {
        Self {
            stop: AtomicBool::new(false),
            enabled: AtomicBool::new(enabled),
            channels: AtomicU8::new(channels.clamp(1, 2)),
            mute_host: AtomicBool::new(mute_host),
            lossless: AtomicBool::new(lossless),
            max_rate_hz: AtomicU32::new(0),
        }
    }
}

pub struct CaptureLoop;

impl CaptureLoop {
    /// Runs the capture loop and emits encoded packets as `(codec, seq, channels, sample_rate, payload)`.
    pub fn run(shared: &CaptureState, mut emit: impl FnMut(u8, u16, u8, u32, &[u8]) -> bool) -> Result<()> {
        let _com = ComGuard::init().context("COM init for audio capture")?;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.context("MMDeviceEnumerator")?;
        let mut mute_guard = HostMuteGuard::new(enumerator.clone());
        let mut encoder = Encoder::new(2)?;
        let mut resampler = DownmixResampler::new();
        let mut gate = SilenceGate::new();
        let mut pcm_acc: Vec<f32> = Vec::new();
        let mut opus_buf = [0u8; MAX_PACKET_BYTES];
        let mut pcm24_buf: Vec<u8> = Vec::new();
        let mut seq: u16 = 0;

        while !shared.stop.load(Ordering::Relaxed) {
            mute_guard.set_muted(shared.mute_host.load(Ordering::Relaxed));
            if !shared.enabled.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
                mute_guard.poll();
                continue;
            }

            let _timer = super::TimerPeriodGuard::enter(1);

            let cur_lossless = shared.lossless.load(Ordering::Relaxed);
            let cur_max_rate = shared.max_rate_hz.load(Ordering::Relaxed);
            let default_info = default_render_rate(&enumerator);
            let host_rate = default_info.as_ref().map(|(_, r)| *r).unwrap_or(SAMPLE_RATE);
            let target_rate = if cur_lossless {
                effective_lossless_rate(host_rate, cur_max_rate)
            } else {
                SAMPLE_RATE
            };
            let cur_id = default_info.map(|(id, _)| id);

            let (audio_client, fmt, block_align, using_process_loopback) =
                match open_process_loopback(target_rate) {
                    Ok((ac, fmt, align)) => {
                        log!(
                            "audio capture: process loopback {} Hz, {} ch ({})",
                            fmt.rate,
                            fmt.channels,
                            if cur_lossless { "lossless PCM24" } else { "Opus" }
                        );
                        (ac, fmt, align, true)
                    }
                    Err(e) => {
                        log!("audio capture: process loopback unavailable ({e:#}); using endpoint loopback");
                        let (ac, fmt, align, _) = open_endpoint_loopback(&enumerator, target_rate)?;
                        log!(
                            "audio capture: endpoint loopback {} Hz, {} ch ({:?})",
                            fmt.rate,
                            fmt.channels,
                            fmt.sample
                        );
                        (ac, fmt, align, false)
                    }
                };

            let cap_client: IAudioCaptureClient =
                unsafe { audio_client.GetService() }.context("GetService IAudioCaptureClient")?;
            unsafe { audio_client.Start() }.context("IAudioClient::Start loopback")?;

            let mut last_ch = shared.channels.load(Ordering::Relaxed).clamp(1, 2);
            let mut last_dev_check = Instant::now();
            let frame_samples = if cur_lossless {
                (target_rate as usize / 100).max(1) // 10 ms frames
            } else {
                FRAME_SAMPLES // 20 ms frames at 48 kHz
            };
            let hangover_frames = if cur_lossless { 50 } else { 25 }; // 500 ms hangover
            pcm_acc.clear();
            resampler.reset();
            gate.reset();

            'reopen: while !shared.stop.load(Ordering::Relaxed) && shared.enabled.load(Ordering::Relaxed) {
                mute_guard.set_muted(shared.mute_host.load(Ordering::Relaxed));
                if shared.lossless.load(Ordering::Relaxed) != cur_lossless {
                    break 'reopen;
                }
                if cur_lossless && shared.max_rate_hz.load(Ordering::Relaxed) != cur_max_rate {
                    break 'reopen;
                }
                let ch = shared.channels.load(Ordering::Relaxed).clamp(1, 2);
                if ch != last_ch {
                    last_ch = ch;
                    pcm_acc.clear();
                    resampler.reset();
                    gate.reset();
                }

                loop {
                    let pkt_frames = match unsafe { cap_client.GetNextPacketSize() } {
                        Ok(n) => n,
                        Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => break 'reopen,
                        Err(e) => {
                            let _ = unsafe { audio_client.Stop() };
                            return Err(e.into());
                        }
                    };
                    if pkt_frames == 0 {
                        break;
                    }
                    let mut data = ptr::null_mut();
                    let mut num_frames = 0u32;
                    let mut flags = 0u32;
                    match unsafe { cap_client.GetBuffer(&mut data, &mut num_frames, &mut flags, None, None) } {
                        Ok(()) => {}
                        Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => break 'reopen,
                        Err(e) => {
                            let _ = unsafe { audio_client.Stop() };
                            return Err(e.into());
                        }
                    }
                    if num_frames > 0 {
                        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                            resampler.reset();
                            let out_frames = (num_frames as u64 * target_rate as u64 / fmt.rate as u64) as usize;
                            pcm_acc.resize(pcm_acc.len() + out_frames * ch as usize, 0.0);
                        } else {
                            let byte_len = num_frames as usize * block_align;
                            let raw = unsafe { std::slice::from_raw_parts(data, byte_len) };
                            resampler.process_raw(raw, fmt, target_rate, ch as usize, &mut pcm_acc);
                        }
                    }
                    if let Err(e) = unsafe { cap_client.ReleaseBuffer(num_frames) } {
                        if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                            break 'reopen;
                        }
                        let _ = unsafe { audio_client.Stop() };
                        return Err(e.into());
                    }
                }

                let frame_floats = frame_samples * ch as usize;
                // Bound accumulation if capture thread is delayed.
                if pcm_acc.len() > frame_floats * 12 {
                    let drop_n = ((pcm_acc.len() - frame_floats * 2) / ch as usize) * ch as usize;
                    pcm_acc.drain(..drop_n);
                }
                let mut consumed = 0;
                while pcm_acc.len() - consumed >= frame_floats {
                    let frame = &pcm_acc[consumed..consumed + frame_floats];
                    if gate.check_hangover(frame, hangover_frames) {
                        if cur_lossless {
                            encode_pcm24(frame, &mut pcm24_buf);
                            if !emit(CODEC_PCM24, seq, ch, target_rate, &pcm24_buf) {
                                let _ = unsafe { audio_client.Stop() };
                                return Ok(());
                            }
                            seq = seq.wrapping_add(1);
                        } else if let Ok(len) = encoder.encode(ch, frame, &mut opus_buf) {
                            if !emit(CODEC_OPUS, seq, ch, SAMPLE_RATE, &opus_buf[..len]) {
                                let _ = unsafe { audio_client.Stop() };
                                return Ok(());
                            }
                            seq = seq.wrapping_add(1);
                        }
                    }
                    consumed += frame_floats;
                }
                if consumed > 0 {
                    pcm_acc.drain(..consumed);
                }

                if last_dev_check.elapsed() >= Duration::from_secs(1) {
                    last_dev_check = Instant::now();
                    mute_guard.poll();
                    let latest = default_render_rate(&enumerator);
                    if !using_process_loopback
                        && latest.as_ref().map(|(id, _)| id.as_str()) != cur_id.as_deref()
                    {
                        log!("audio capture: default output device switched");
                        break 'reopen;
                    }
                    if cur_lossless
                        && let Some((_, r)) = latest
                        && effective_lossless_rate(r, cur_max_rate) != target_rate
                    {
                        log!(
                            "audio capture: effective lossless rate changed ({target_rate} -> {} Hz)",
                            effective_lossless_rate(r, cur_max_rate)
                        );
                        break 'reopen;
                    }
                }

                std::thread::sleep(Duration::from_millis(4));
            }

            let _ = unsafe { audio_client.Stop() };
            if !shared.stop.load(Ordering::Relaxed) && shared.enabled.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Ok(())
    }
}

/// Reads framed lossless PCM24 packets from a `STREAM_AUDIO` unidirectional QUIC stream
/// and forwards them into the viewer's audio player channel.
pub async fn recv_lossless_stream(mut r: RecvStream, tx: UnboundedSender<Bytes>) {
    let mut len_buf = [0u8; 4];
    loop {
        if r.read_exact(&mut len_buf).await.is_err() {
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if !(8..=MAX_LOSSLESS_FRAME).contains(&len) {
            let _ = r.stop(0u32.into());
            break;
        }
        let mut body = vec![0u8; len];
        if r.read_exact(&mut body).await.is_err() {
            break;
        }
        if body[0] != CODEC_PCM24 {
            continue;
        }
        // Replace codec tag with internal wire discriminator T_AUDIO_PCM24 in-place.
        body[0] = T_AUDIO_PCM24;
        if tx.send(Bytes::from(body)).is_err() {
            let _ = r.stop(0u32.into());
            break;
        }
    }
}

struct ParsedPkt {
    codec: u8,
    seq: u16,
    ch: u8,
    rate: u32,
    payload: Bytes,
}

fn parse_incoming_pkt(d: Bytes) -> Option<ParsedPkt> {
    match *d.first()? {
        T_AUDIO if d.len() >= 4 => Some(ParsedPkt {
            codec: CODEC_OPUS,
            seq: u16::from_be_bytes([d[1], d[2]]),
            ch: d[3].clamp(1, 2),
            rate: SAMPLE_RATE,
            payload: d.slice(4..),
        }),
        T_AUDIO_PCM24 if d.len() >= 8 => {
            let rate = normalize_lossless_rate(u32::from_be_bytes([d[4], d[5], d[6], d[7]]));
            Some(ParsedPkt {
                codec: CODEC_PCM24,
                seq: u16::from_be_bytes([d[1], d[2]]),
                ch: d[3].clamp(1, 2),
                rate,
                payload: d.slice(8..),
            })
        }
        _ => None,
    }
}

fn drain_rx(rx: &mut UnboundedReceiver<Bytes>) -> bool {
    loop {
        match rx.try_recv() {
            Ok(_) => {}
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

/// Keeps at most ~2 ms of buffered audio and fades it linearly to 0.0 so codec
/// switches do not click or pop.
fn fade_out_tail(pcm_queue: &mut VecDeque<f32>, rate: u32, ch: usize) {
    if ch == 0 {
        pcm_queue.clear();
        return;
    }
    let fade_frames = (rate as usize / 500).max(1);
    let keep_floats = fade_frames * ch;
    if pcm_queue.len() > keep_floats {
        pcm_queue.truncate(keep_floats);
    }
    let actual_frames = pcm_queue.len() / ch;
    if actual_frames == 0 {
        pcm_queue.clear();
        return;
    }
    for f in 0..actual_frames {
        let gain = 1.0 - (f + 1) as f32 / actual_frames as f32;
        for c in 0..ch {
            if let Some(s) = pcm_queue.get_mut(f * ch + c) {
                *s *= gain;
            }
        }
    }
}

/// Lightweight adaptive jitter buffer controller for viewer audio playback.
struct JitterCtrl {
    target_ms: u32,
    last_bump: Option<Instant>,
    last_decay: Option<Instant>,
    gap_since: Option<Instant>,
}

impl JitterCtrl {
    const BASE_MS: u32 = 10;
    const MAX_MS: u32 = 45;
    const STEP_UP_MS: u32 = 10;
    const DECAY_STEP_MS: u32 = 5;
    const LOW_BUFFER_MS: u32 = 12;
    const WASAPI_HEADROOM_MS: u32 = 25;
    const STABLE_HOLD: Duration = Duration::from_secs(15);
    const DECAY_INTERVAL: Duration = Duration::from_secs(5);

    fn new() -> Self {
        Self {
            target_ms: Self::BASE_MS,
            last_bump: None,
            last_decay: None,
            gap_since: None,
        }
    }

    fn target_ms(&self) -> u32 {
        self.target_ms
    }

    fn wasapi_ceiling_ms(&self) -> u32 {
        self.target_ms() + Self::WASAPI_HEADROOM_MS
    }

    fn prime_params(&self, frame_ms: u32) -> (usize, Duration) {
        let frame_ms = frame_ms.max(1);
        let packets = (self.target_ms / frame_ms).max(1) as usize;
        let timeout = Duration::from_millis(self.target_ms.max(20) as u64);
        (packets, timeout)
    }

    fn max_pending(&self, codec: u8) -> usize {
        let (base_max, frame_ms) = if codec == CODEC_PCM24 {
            (10usize, 10u32)
        } else {
            (5usize, 20u32)
        };
        let extra = self.target_ms.saturating_sub(Self::BASE_MS).div_ceil(frame_ms) as usize;
        base_max + extra
    }

    fn on_jitter_event(&mut self, now: Instant) {
        self.target_ms = (self.target_ms + Self::STEP_UP_MS).min(Self::MAX_MS);
        self.last_bump = Some(now);
        self.last_decay = None;
    }

    fn tick(&mut self, now: Instant) {
        if self.target_ms <= Self::BASE_MS {
            return;
        }
        let Some(bumped) = self.last_bump else {
            return;
        };
        if now.saturating_duration_since(bumped) < Self::STABLE_HOLD {
            return;
        }
        let due = self
            .last_decay
            .is_none_or(|prev| now.saturating_duration_since(prev) >= Self::DECAY_INTERVAL);
        if due {
            self.target_ms = self
                .target_ms
                .saturating_sub(Self::DECAY_STEP_MS)
                .max(Self::BASE_MS);
            self.last_decay = Some(now);
        }
    }

    fn should_wait_gap(&mut self, now: Instant, buffered_ms: u32) -> bool {
        if buffered_ms <= Self::LOW_BUFFER_MS {
            self.gap_since = None;
            return false;
        }
        let since = *self.gap_since.get_or_insert(now);
        if now.saturating_duration_since(since) < Duration::from_millis(self.target_ms as u64) {
            true
        } else {
            self.gap_since = None;
            false
        }
    }

    fn clear_gap(&mut self) {
        self.gap_since = None;
    }

    fn reset_stream(&mut self) {
        self.gap_since = None;
    }
}

/// Maps UI volume percentage `0..=100` to linear amplitude using a quadratic
/// perceptual curve (`(v/100)^2`), giving fine control at low background levels.
fn volume_gain(pct: u8) -> f32 {
    if pct == 0 {
        0.0
    } else if pct >= 100 {
        1.0
    } else {
        let x = pct as f32 / 100.0;
        x * x
    }
}

pub struct Player;

impl Player {
    /// Runs the audio player with an explicit `reset` signal (set on reconnect or
    /// host handoff so sequence numbers restarting from 0 are accepted immediately).
    pub fn run_with_reset(mut rx: UnboundedReceiver<Bytes>, stop: Arc<AtomicBool>, reset: Arc<AtomicBool>) {
        if let Err(e) = Self::run_inner(&mut rx, &stop, &reset) {
            log!("audio playback ended: {e:#}");
        }
    }

    fn run_inner(rx: &mut UnboundedReceiver<Bytes>, stop: &AtomicBool, reset: &AtomicBool) -> Result<()> {
        let _com = ComGuard::init().context("COM init for audio playback")?;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.context("MMDeviceEnumerator")?;

        let mut decoder = Decoder::new(2)?;
        let mut resampler = DownmixResampler::new();
        let mut pending: BTreeMap<i64, (u8, u8, Bytes)> = BTreeMap::new();
        let mut pcm_queue: VecDeque<f32> = VecDeque::new();
        let mut decode_buf = vec![0.0f32; FRAME_SAMPLES * 2];
        let mut pcm24_f32: Vec<f32> = Vec::new();
        let mut resampled_buf: Vec<f32> = Vec::new();
        let mut last_rx: Option<(u16, i64)> = None;
        let mut expected_seq: Option<i64> = None;
        let mut last_rx_at: Option<Instant> = None;
        let mut desired_rate: u32 = SAMPLE_RATE;
        let mut cur_codec: u8 = CODEC_OPUS;
        let mut jitter = JitterCtrl::new();

        while !stop.load(Ordering::Relaxed) {
            if reset.swap(false, Ordering::AcqRel) {
                pending.clear();
                pcm_queue.clear();
                expected_seq = None;
                last_rx = None;
                last_rx_at = None;
                jitter.reset_stream();
                resampler.reset();
            }

            let Ok((dev, cur_id)) = default_render_device(&enumerator) else {
                if !drain_rx(rx) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            };

            let Ok(audio_client) = (unsafe { dev.Activate::<IAudioClient>(CLSCTX_ALL, None) }) else {
                if !drain_rx(rx) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            };

            if let Ok(mix_ptr) = unsafe { audio_client.GetMixFormat() } {
                let viewer_rate = unsafe { (*mix_ptr).nSamplesPerSec };
                unsafe { CoTaskMemFree(Some(mix_ptr as *const _)) };
                super::config::set_viewer_rate_hz(viewer_rate);
            }

            let active_rate = desired_rate;
            let req_wfx = stereo_f32_wfx(active_rate);
            let auto_flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            let fmt = if unsafe {
                audio_client.Initialize(AUDCLNT_SHAREMODE_SHARED, auto_flags, 1_000_000, 0, &req_wfx, None)
            }
            .is_ok()
            {
                log!("audio playback: {} Hz, 2 ch (F32, Windows SRC/upmix)", active_rate);
                PcmFormat { rate: active_rate, channels: 2, sample: SampleFmt::F32 }
            } else {
                let Ok(mix_ptr) = (unsafe { audio_client.GetMixFormat() }) else {
                    if !drain_rx(rx) {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                };
                let fmt_res = parse_mix_format(mix_ptr);
                let init_res = fmt_res.and_then(|(fmt, _)| {
                    unsafe { audio_client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, mix_ptr, None) }?;
                    Ok(fmt)
                });
                unsafe { CoTaskMemFree(Some(mix_ptr as *const _)) };
                let Ok(fmt) = init_res else {
                    if !drain_rx(rx) {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                };
                log!("audio playback: {} Hz, {} ch ({:?}, native mix fallback)", fmt.rate, fmt.channels, fmt.sample);
                fmt
            };

            super::config::set_active_rate_hz(active_rate);
            let buffer_frames = unsafe { audio_client.GetBufferSize() }?;
            let render_client: IAudioRenderClient = unsafe { audio_client.GetService() }?;
            unsafe { audio_client.Start() }?;

            resampler.reset();
            pcm_queue.clear();
            jitter.reset_stream();
            let mut primed = false;
            let mut first_pending_at: Option<Instant> = None;
            let mut underrun_armed = false;
            let mut last_dev_check = Instant::now();
            let mut timer_guard: Option<super::TimerPeriodGuard> = None;
            let mut cur_gain = volume_gain(super::config::viewer_volume());
            let dev_ch = fmt.channels as usize;
            let dev_10ms_floats = (fmt.rate as usize / 100).max(1) * dev_ch;
            let max_queue_floats = dev_10ms_floats * 12; // ~120 ms cap

            'reopen: while !stop.load(Ordering::Relaxed) {
                if reset.swap(false, Ordering::AcqRel) {
                    pending.clear();
                    pcm_queue.clear();
                    expected_seq = None;
                    last_rx = None;
                    last_rx_at = None;
                    primed = false;
                    first_pending_at = None;
                    underrun_armed = false;
                    jitter.reset_stream();
                    resampler.reset();
                }

                loop {
                    match rx.try_recv() {
                        Ok(d) => {
                            let Some(pkt) = parse_incoming_pkt(d) else {
                                continue;
                            };
                            if !super::config::viewer_enabled() {
                                continue;
                            }
                            if pcm_queue.is_empty()
                                && pending.is_empty()
                                && last_rx_at.is_some_and(|t| t.elapsed() >= Duration::from_millis(250))
                            {
                                expected_seq = None;
                                last_rx = None;
                                primed = false;
                                first_pending_at = None;
                                underrun_armed = false;
                                jitter.reset_stream();
                            }
                            last_rx_at = Some(Instant::now());
                            // Drop stale in-flight packets from the previous codec so Opus <-> PCM24
                            // transitions never ping-pong and flush the playback queue.
                            if pkt.codec != cur_codec
                                && let Some((prev_u16, _)) = last_rx
                                && (pkt.seq.wrapping_sub(prev_u16) as i16) <= 0
                            {
                                continue;
                            }
                            if pkt.rate != active_rate {
                                desired_rate = pkt.rate;
                                cur_codec = pkt.codec;
                                pending.clear();
                                pcm_queue.clear();
                                resampler.reset();
                                jitter.reset_stream();
                                last_rx = Some((pkt.seq, pkt.seq as i64));
                                expected_seq = Some(pkt.seq as i64);
                                pending.insert(pkt.seq as i64, (pkt.codec, pkt.ch, pkt.payload));
                                break 'reopen;
                            }
                            if pkt.codec != cur_codec {
                                cur_codec = pkt.codec;
                                pending.clear();
                                fade_out_tail(&mut pcm_queue, fmt.rate, dev_ch);
                                resampler.reset();
                                last_rx = None;
                                expected_seq = None;
                                primed = false;
                                first_pending_at = Some(Instant::now());
                                underrun_armed = false;
                                jitter.reset_stream();
                            }
                            let u_seq = match last_rx {
                                None => {
                                    last_rx = Some((pkt.seq, pkt.seq as i64));
                                    pkt.seq as i64
                                }
                                Some((prev_u16, prev_u)) => {
                                    let diff = pkt.seq.wrapping_sub(prev_u16) as i16 as i64;
                                    let val = prev_u + diff;
                                    if diff > 0 {
                                        last_rx = Some((pkt.seq, val));
                                    }
                                    val
                                }
                            };
                            match expected_seq {
                                None => {
                                    expected_seq = Some(u_seq);
                                    primed = false;
                                    first_pending_at = Some(Instant::now());
                                    jitter.reset_stream();
                                }
                                Some(e) if (u_seq - e).abs() > 100 => {
                                    pending.clear();
                                    pcm_queue.clear();
                                    resampler.reset();
                                    expected_seq = Some(u_seq);
                                    primed = false;
                                    first_pending_at = Some(Instant::now());
                                    underrun_armed = false;
                                    jitter.reset_stream();
                                }
                                _ => {}
                            }
                            if underrun_armed {
                                jitter.on_jitter_event(Instant::now());
                                underrun_armed = false;
                            }
                            if let Some(e) = expected_seq {
                                if u_seq >= e {
                                    pending.entry(u_seq).or_insert((pkt.codec, pkt.ch, pkt.payload));
                                    if !primed && first_pending_at.is_none() {
                                        first_pending_at = Some(Instant::now());
                                    }
                                } else {
                                    jitter.on_jitter_event(Instant::now());
                                }
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            let _ = unsafe { audio_client.Stop() };
                            return Ok(());
                        }
                    }
                }

                let padding = match unsafe { audio_client.GetCurrentPadding() } {
                    Ok(p) => p,
                    Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => break 'reopen,
                    Err(e) => {
                        let _ = unsafe { audio_client.Stop() };
                        return Err(e.into());
                    }
                };

                let now = Instant::now();
                let enabled = super::config::viewer_enabled();
                let mut target_wasapi_frames = 0usize;
                if !enabled {
                    timer_guard = None;
                    cur_gain = 0.0;
                    pending.clear();
                    pcm_queue.clear();
                    expected_seq = None;
                    last_rx = None;
                    last_rx_at = None;
                    primed = false;
                    first_pending_at = None;
                    underrun_armed = false;
                    jitter.reset_stream();
                } else {
                    if timer_guard.is_none() {
                        timer_guard = Some(super::TimerPeriodGuard::enter(1));
                    }
                    jitter.tick(now);
                    let frame_ms = if cur_codec == CODEC_PCM24 { 10 } else { 20 };
                    let (prime_packets, prime_timeout) = jitter.prime_params(frame_ms);
                    let max_pending = jitter.max_pending(cur_codec);
                    target_wasapi_frames = ((fmt.rate as usize * jitter.wasapi_ceiling_ms() as usize) / 1000)
                        .min(buffer_frames as usize);

                    if !primed
                        && (pending.len() >= prime_packets
                            || first_pending_at.is_some_and(|t| t.elapsed() >= prime_timeout))
                    {
                        primed = true;
                    }
                    if primed {
                        while pending.len() > max_pending {
                            if let Some((&k, _)) = pending.first_key_value() {
                                pending.remove(&k);
                                expected_seq = Some(k + 1);
                                jitter.clear_gap();
                            }
                        }
                        while (padding as usize + (pcm_queue.len() / dev_ch)) < target_wasapi_frames
                            && pcm_queue.len() < max_queue_floats
                        {
                            let Some(exp) = expected_seq else { break };
                            if let Some((codec, ch, payload)) = pending.remove(&exp) {
                                jitter.clear_gap();
                                if codec == CODEC_PCM24 {
                                    decode_pcm24(&payload, &mut pcm24_f32);
                                    if ch as usize == dev_ch && active_rate == fmt.rate {
                                        pcm_queue.extend(pcm24_f32.iter().copied());
                                    } else {
                                        resampled_buf.clear();
                                        resampler.process_f32(
                                            &pcm24_f32,
                                            active_rate,
                                            ch as usize,
                                            fmt.rate,
                                            dev_ch,
                                            &mut resampled_buf,
                                        );
                                        pcm_queue.extend(resampled_buf.iter().copied());
                                    }
                                } else if let Ok(n) = decoder.decode(ch, &payload, &mut decode_buf) {
                                    resampled_buf.clear();
                                    resampler.process_f32(
                                        &decode_buf[..n * ch as usize],
                                        SAMPLE_RATE,
                                        ch as usize,
                                        fmt.rate,
                                        dev_ch,
                                        &mut resampled_buf,
                                    );
                                    pcm_queue.extend(resampled_buf.iter().copied());
                                }
                                expected_seq = Some(exp + 1);
                            } else if let Some(&next_k) = pending.keys().next() {
                                let gap = next_k - exp;
                                if gap <= 0 {
                                    pending.remove(&next_k);
                                    continue;
                                }
                                let sw_frames = pcm_queue.len() / dev_ch;
                                let buffered_ms =
                                    (((padding as usize + sw_frames) as u64 * 1000) / fmt.rate as u64) as u32;
                                if jitter.should_wait_gap(now, buffered_ms) {
                                    break;
                                }
                                jitter.on_jitter_event(now);
                                if cur_codec == CODEC_OPUS && (1..=3).contains(&gap) {
                                    if decoder.decode_plc(&mut decode_buf).is_ok() {
                                        let ch = decoder.channels();
                                        resampled_buf.clear();
                                        resampler.process_f32(
                                            &decode_buf[..FRAME_SAMPLES * ch as usize],
                                            SAMPLE_RATE,
                                            ch as usize,
                                            fmt.rate,
                                            dev_ch,
                                            &mut resampled_buf,
                                        );
                                        pcm_queue.extend(resampled_buf.iter().copied());
                                    }
                                    expected_seq = Some(exp + 1);
                                } else {
                                    expected_seq = Some(next_k);
                                }
                            } else {
                                break;
                            }
                        }
                        if pcm_queue.len() > max_queue_floats {
                            let drop_n = ((pcm_queue.len() - max_queue_floats) / dev_ch) * dev_ch;
                            pcm_queue.drain(..drop_n);
                        }
                        if pcm_queue.is_empty() && pending.is_empty() {
                            if last_rx_at.is_some_and(|t| t.elapsed() >= Duration::from_millis(250)) {
                                expected_seq = None;
                                last_rx = None;
                                primed = false;
                                first_pending_at = None;
                                underrun_armed = false;
                                jitter.reset_stream();
                            } else if padding == 0
                                && last_rx_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(120))
                            {
                                primed = false;
                                first_pending_at = None;
                                underrun_armed = false;
                                jitter.reset_stream();
                            }
                        }
                    }
                }

                let avail = target_wasapi_frames.saturating_sub(padding as usize);
                let queued_frames = pcm_queue.len() / dev_ch;
                let frames_to_write = avail.min(queued_frames);

                if frames_to_write > 0 {
                    let buf_ptr = match unsafe { render_client.GetBuffer(frames_to_write as u32) } {
                        Ok(p) => p,
                        Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => break 'reopen,
                        Err(e) => {
                            let _ = unsafe { audio_client.Stop() };
                            return Err(e.into());
                        }
                    };
                    let n_floats = frames_to_write * dev_ch;
                    let target_gain = volume_gain(super::config::viewer_volume());
                    let gain_step = 1.0 / (fmt.rate as f32 * 0.015).max(1.0);
                    let mut next_frame_gain = || {
                        if (cur_gain - target_gain).abs() <= gain_step {
                            cur_gain = target_gain;
                        } else if cur_gain < target_gain {
                            cur_gain += gain_step;
                        } else {
                            cur_gain -= gain_step;
                        }
                        cur_gain
                    };
                    match fmt.sample {
                        SampleFmt::F32 => {
                            let dst = unsafe { std::slice::from_raw_parts_mut(buf_ptr as *mut f32, n_floats) };
                            for frame in dst.chunks_exact_mut(dev_ch) {
                                let g = next_frame_gain();
                                for slot in frame {
                                    *slot = pcm_queue.pop_front().unwrap_or(0.0) * g;
                                }
                            }
                        }
                        SampleFmt::I16 => {
                            let dst = unsafe { std::slice::from_raw_parts_mut(buf_ptr as *mut i16, n_floats) };
                            for frame in dst.chunks_exact_mut(dev_ch) {
                                let g = next_frame_gain();
                                for slot in frame {
                                    let s = (pcm_queue.pop_front().unwrap_or(0.0) * g).clamp(-1.0, 1.0);
                                    *slot = (s * 32767.0) as i16;
                                }
                            }
                        }
                        SampleFmt::I24 => {
                            let dst = unsafe { std::slice::from_raw_parts_mut(buf_ptr, n_floats * 3) };
                            for frame in dst.chunks_exact_mut(dev_ch * 3) {
                                let g = next_frame_gain();
                                for tri in frame.chunks_exact_mut(3) {
                                    let s = (pcm_queue.pop_front().unwrap_or(0.0) * g).clamp(-1.0, 1.0);
                                    let q = (s * 8_388_607.0).round() as i32;
                                    let b = q.to_le_bytes();
                                    tri.copy_from_slice(&b[..3]);
                                }
                            }
                        }
                        SampleFmt::I32 => {
                            let dst = unsafe { std::slice::from_raw_parts_mut(buf_ptr as *mut i32, n_floats) };
                            for frame in dst.chunks_exact_mut(dev_ch) {
                                let g = next_frame_gain();
                                for slot in frame {
                                    let s = (pcm_queue.pop_front().unwrap_or(0.0) * g).clamp(-1.0, 1.0);
                                    *slot = (s as f64 * 2_147_483_647.0).round() as i32;
                                }
                            }
                        }
                    }
                    if let Err(e) = unsafe { render_client.ReleaseBuffer(frames_to_write as u32, 0) } {
                        if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                            break 'reopen;
                        }
                        let _ = unsafe { audio_client.Stop() };
                        return Err(e.into());
                    }
                } else if enabled && primed && padding == 0 && pcm_queue.is_empty() {
                    underrun_armed = true;
                }

                if last_dev_check.elapsed() >= Duration::from_secs(1) {
                    last_dev_check = Instant::now();
                    if let Some((id, r)) = default_render_rate(&enumerator) {
                        super::config::set_viewer_rate_hz(r);
                        if id != cur_id {
                            log!("audio playback: default output device switched");
                            break 'reopen;
                        }
                    }
                }

                std::thread::sleep(Duration::from_millis(4));
            }

            let _ = unsafe { audio_client.Stop() };
            if !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(80));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_gain_is_monotonic_and_bounded() {
        assert_eq!(volume_gain(0), 0.0);
        assert_eq!(volume_gain(100), 1.0);
        assert_eq!(volume_gain(255), 1.0);
        assert!((volume_gain(50) - 0.25).abs() < 1e-6);
        assert!(volume_gain(20) > 0.0 && volume_gain(20) < volume_gain(50));
    }

    #[test]
    fn effective_lossless_rate_picks_min_of_endpoints() {
        assert_eq!(effective_lossless_rate(48_000, 192_000), 48_000);
        assert_eq!(effective_lossless_rate(192_000, 48_000), 48_000);
        assert_eq!(effective_lossless_rate(192_000, 192_000), 192_000);
        assert_eq!(effective_lossless_rate(48_000, 44_100), 44_100);
        assert_eq!(effective_lossless_rate(44_100, 48_000), 44_100);
        assert_eq!(effective_lossless_rate(88_200, 48_000), 48_000);
        assert_eq!(effective_lossless_rate(96_000, 0), 96_000);
    }

    #[test]
    fn jitter_ctrl_starts_at_base_and_caps_at_max() {
        let t0 = Instant::now();
        let mut jc = JitterCtrl::new();
        assert_eq!(jc.target_ms(), JitterCtrl::BASE_MS);
        assert_eq!(jc.wasapi_ceiling_ms(), JitterCtrl::BASE_MS + JitterCtrl::WASAPI_HEADROOM_MS);
        assert_eq!(jc.prime_params(20), (1, Duration::from_millis(20)));
        assert_eq!(jc.prime_params(10), (1, Duration::from_millis(20)));
        assert_eq!(jc.max_pending(CODEC_OPUS), 5);
        assert_eq!(jc.max_pending(CODEC_PCM24), 10);

        jc.on_jitter_event(t0);
        assert_eq!(jc.target_ms(), 20);
        jc.on_jitter_event(t0 + Duration::from_millis(10));
        assert_eq!(jc.target_ms(), 30);
        jc.on_jitter_event(t0 + Duration::from_millis(20));
        assert_eq!(jc.target_ms(), 40);
        jc.on_jitter_event(t0 + Duration::from_millis(30));
        assert_eq!(jc.target_ms(), JitterCtrl::MAX_MS);
        jc.on_jitter_event(t0 + Duration::from_millis(40));
        assert_eq!(jc.target_ms(), JitterCtrl::MAX_MS);
        assert_eq!(jc.prime_params(20), (2, Duration::from_millis(45)));
        assert_eq!(jc.max_pending(CODEC_OPUS), 7);
        assert_eq!(jc.max_pending(CODEC_PCM24), 14);
    }

    #[test]
    fn jitter_ctrl_decays_slowly_after_stable_period() {
        let t0 = Instant::now();
        let mut jc = JitterCtrl::new();
        jc.on_jitter_event(t0);
        jc.on_jitter_event(t0);
        assert_eq!(jc.target_ms(), 30);

        jc.tick(t0 + Duration::from_secs(14));
        assert_eq!(jc.target_ms(), 30);

        jc.tick(t0 + Duration::from_secs(15));
        assert_eq!(jc.target_ms(), 25);

        jc.tick(t0 + Duration::from_secs(19));
        assert_eq!(jc.target_ms(), 25);

        jc.tick(t0 + Duration::from_secs(20));
        assert_eq!(jc.target_ms(), 20);

        // New event resets stability hold.
        let t1 = t0 + Duration::from_secs(21);
        jc.on_jitter_event(t1);
        assert_eq!(jc.target_ms(), 30);
        jc.tick(t1 + Duration::from_secs(10));
        assert_eq!(jc.target_ms(), 30);

        jc.tick(t1 + Duration::from_secs(15));
        assert_eq!(jc.target_ms(), 25);
        jc.tick(t1 + Duration::from_secs(20));
        assert_eq!(jc.target_ms(), 20);
        jc.tick(t1 + Duration::from_secs(25));
        assert_eq!(jc.target_ms(), 15);
        jc.tick(t1 + Duration::from_secs(30));
        assert_eq!(jc.target_ms(), JitterCtrl::BASE_MS);
        jc.tick(t1 + Duration::from_secs(40));
        assert_eq!(jc.target_ms(), JitterCtrl::BASE_MS);
    }

    #[test]
    fn jitter_ctrl_gap_wait_respects_buffer_and_deadline() {
        let t0 = Instant::now();
        let mut jc = JitterCtrl::new();
        jc.on_jitter_event(t0);
        assert_eq!(jc.target_ms(), 20);

        // Buffered audio sufficient (> 12 ms): wait until target_ms expires.
        assert!(jc.should_wait_gap(t0, 25));
        assert!(jc.should_wait_gap(t0 + Duration::from_millis(12), 20));
        assert!(!jc.should_wait_gap(t0 + Duration::from_millis(20), 20));

        // Low buffer (<= 12 ms): release immediately to prevent underrun.
        let t1 = t0 + Duration::from_millis(30);
        assert!(jc.should_wait_gap(t1, 25));
        assert!(!jc.should_wait_gap(t1 + Duration::from_millis(4), 12));

        // Late packet arrival clears gap timer cleanly.
        let t2 = t0 + Duration::from_millis(50);
        assert!(jc.should_wait_gap(t2, 25));
        jc.clear_gap();
        let t3 = t2 + Duration::from_millis(100);
        assert!(jc.should_wait_gap(t3, 25));
    }

    #[test]
    fn process_loopback_opens_and_reads() {
        let _com = ComGuard::init().unwrap();
        let res = open_process_loopback(48_000);
        eprintln!("PROCESS_LOOPBACK_OPEN: {:?}", res.as_ref().map(|(_, f, a)| (*f, *a)));
        let (ac, _fmt, _align) = res.expect("open_process_loopback failed");
        let cap: IAudioCaptureClient = unsafe { ac.GetService() }.unwrap();
        unsafe { ac.Start() }.unwrap();
        let mut total_pkts = 0u32;
        let mut total_frames = 0u32;
        let mut silent_pkts = 0u32;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(10));
            while let Ok(sz) = unsafe { cap.GetNextPacketSize() } {
                if sz == 0 {
                    break;
                }
                let mut data = ptr::null_mut();
                let mut n = 0u32;
                let mut flags = 0u32;
                unsafe { cap.GetBuffer(&mut data, &mut n, &mut flags, None, None) }.unwrap();
                total_pkts += 1;
                total_frames += n;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    silent_pkts += 1;
                }
                unsafe { cap.ReleaseBuffer(n) }.unwrap();
            }
        }
        let _ = unsafe { ac.Stop() };
        eprintln!("PROCESS_LOOPBACK_READ: pkts={total_pkts} frames={total_frames} silent={silent_pkts}");
    }
}
