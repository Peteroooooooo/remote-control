//! Host-side audio controller (`AudioHost`) and user-session `audio-agent`
//! subcommand for service mode.

use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use anyhow::Result;
use bytes::Bytes;
use iroh::endpoint::{Connection, SendStream};
use rc_stream::wire::T_AUDIO;

use super::{
    codec::{CODEC_OPUS, CODEC_PCM24},
    wasapi::{CaptureLoop, CaptureState},
};
use crate::{
    log,
    proto::{AudioCfg, STREAM_AUDIO},
    term_proc,
};

const CMD_CONFIG: u8 = 1;
const CMD_STOP: u8 = 2;

fn encode_cmd(cmd: u8, cfg: AudioCfg) -> [u8; 9] {
    let mut pkt = [0u8; 9];
    pkt[0] = cmd;
    pkt[1] = cfg.enabled as u8;
    pkt[2] = cfg.channels.clamp(1, 2);
    pkt[3] = cfg.mute_host as u8;
    pkt[4] = cfg.lossless as u8;
    pkt[5..9].copy_from_slice(&cfg.max_rate_hz.to_be_bytes());
    pkt
}

/// `remote-control audio-agent`: captures system audio in the logged-in user's
/// session and writes framed audio packets to stdout.
pub fn serve() -> Result<()> {
    crate::stats::QUIET.store(true, Ordering::Relaxed);
    let shared = Arc::new(CaptureState::new(false, 2, false, false));
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("audio-agent-in".into())
            .spawn(move || {
                let mut stdin = std::io::stdin().lock();
                let mut cmd = [0u8; 9];
                while stdin.read_exact(&mut cmd).is_ok() {
                    match cmd[0] {
                        CMD_CONFIG => {
                            shared.enabled.store(cmd[1] != 0, Ordering::Relaxed);
                            shared.channels.store(cmd[2].clamp(1, 2), Ordering::Relaxed);
                            shared.mute_host.store(cmd[3] != 0, Ordering::Relaxed);
                            shared.lossless.store(cmd[4] != 0, Ordering::Relaxed);
                            let max_rate = u32::from_be_bytes([cmd[5], cmd[6], cmd[7], cmd[8]]);
                            shared.max_rate_hz.store(max_rate, Ordering::Relaxed);
                        }
                        CMD_STOP => break,
                        _ => {}
                    }
                }
                shared.stop.store(true, Ordering::SeqCst);
            })?;
    }

    let mut out = std::io::stdout().lock();
    let mut frame = Vec::with_capacity(4 + 8 + 16_384);
    CaptureLoop::run(&shared, |codec, seq, ch, rate, payload| {
        let body_len = 8 + payload.len();
        if body_len > 65_536 {
            return true;
        }
        frame.clear();
        frame.extend_from_slice(&(body_len as u32).to_le_bytes());
        frame.push(codec);
        frame.extend_from_slice(&seq.to_be_bytes());
        frame.push(ch);
        frame.extend_from_slice(&rate.to_be_bytes());
        frame.extend_from_slice(payload);
        out.write_all(&frame).is_ok() && out.flush().is_ok()
    })
}

pub struct AudioHost {
    stop: Arc<AtomicBool>,
    state: Arc<CaptureState>,
    cfg: Arc<Mutex<AudioCfg>>,
    agent_stdin: Arc<Mutex<Option<File>>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioHost {
    pub fn start(conn: Connection, rt: tokio::runtime::Handle, paused: Arc<AtomicBool>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(CaptureState::new(false, 2, false, false));
        let cfg = Arc::new(Mutex::new(AudioCfg {
            enabled: false,
            channels: 2,
            mute_host: false,
            lossless: false,
            max_rate_hz: 0,
        }));
        let agent_stdin = Arc::new(Mutex::new(None));

        let thread = {
            let (stop, state, cfg, agent_stdin) = (stop.clone(), state.clone(), cfg.clone(), agent_stdin.clone());
            std::thread::Builder::new()
                .name("audio-host".into())
                .spawn(move || {
                    if crate::desktop::enabled() {
                        run_service_agent(conn, rt, paused, stop, cfg, agent_stdin);
                    } else {
                        run_in_process(conn, rt, paused, stop, state);
                    }
                })
                .ok()
        };

        Self { stop, state, cfg, agent_stdin, thread }
    }

    pub fn configure(&self, new_cfg: AudioCfg) {
        let new_cfg = AudioCfg { channels: new_cfg.channels.clamp(1, 2), ..new_cfg };
        *self.cfg.lock().unwrap() = new_cfg;
        self.state.enabled.store(new_cfg.enabled, Ordering::Relaxed);
        self.state.channels.store(new_cfg.channels, Ordering::Relaxed);
        self.state.mute_host.store(new_cfg.mute_host, Ordering::Relaxed);
        self.state.lossless.store(new_cfg.lossless, Ordering::Relaxed);
        self.state.max_rate_hz.store(new_cfg.max_rate_hz, Ordering::Relaxed);

        let mut guard = self.agent_stdin.lock().unwrap();
        if let Some(stdin) = guard.as_mut() {
            let pkt = encode_cmd(CMD_CONFIG, new_cfg);
            if stdin.write_all(&pkt).is_err() || stdin.flush().is_err() {
                *guard = None;
            }
        }
    }
}

impl Drop for AudioHost {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.state.stop.store(true, Ordering::SeqCst);
        if let Some(mut stdin) = self.agent_stdin.lock().unwrap().take() {
            let _ = stdin.write_all(&encode_cmd(CMD_STOP, AudioCfg::default()));
            let _ = stdin.flush();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct AudioSender {
    conn: Connection,
    rt: tokio::runtime::Handle,
    paused: Arc<AtomicBool>,
    stream: Option<SendStream>,
    wire_buf: Vec<u8>,
    /// Monotonic connection-wide sequence number, surviving `audio-agent` restarts.
    tx_seq: u16,
}

impl AudioSender {
    fn new(conn: Connection, rt: tokio::runtime::Handle, paused: Arc<AtomicBool>) -> Self {
        Self { conn, rt, paused, stream: None, wire_buf: Vec::with_capacity(16_384), tx_seq: 0 }
    }

    fn close_stream(&mut self) {
        if let Some(mut s) = self.stream.take() {
            let _ = s.finish();
        }
    }

    fn send(&mut self, codec: u8, _seq: u16, ch: u8, rate: u32, payload: &[u8]) {
        if self.paused.load(Ordering::Relaxed) {
            self.close_stream();
            return;
        }
        let seq = self.tx_seq;
        self.tx_seq = self.tx_seq.wrapping_add(1);
        if codec == CODEC_OPUS {
            self.close_stream();
            let mut pkt = Vec::with_capacity(4 + payload.len());
            pkt.push(T_AUDIO);
            pkt.extend_from_slice(&seq.to_be_bytes());
            pkt.push(ch);
            pkt.extend_from_slice(payload);
            let _ = self.conn.send_datagram(Bytes::from(pkt));
        } else if codec == CODEC_PCM24 {
            let body_len = 8 + payload.len();
            self.wire_buf.clear();
            self.wire_buf.extend_from_slice(&(body_len as u32).to_be_bytes());
            self.wire_buf.push(CODEC_PCM24);
            self.wire_buf.extend_from_slice(&seq.to_be_bytes());
            self.wire_buf.push(ch);
            self.wire_buf.extend_from_slice(&rate.to_be_bytes());
            self.wire_buf.extend_from_slice(payload);

            let conn = self.conn.clone();
            let buf = &self.wire_buf;
            let mut stream = self.stream.take();
            let res: Result<SendStream> = self.rt.block_on(async {
                let mut s = match stream.take() {
                    Some(s) => s,
                    None => {
                        let mut s = conn.open_uni().await?;
                        let _ = s.set_priority(10);
                        s.write_all(&[STREAM_AUDIO]).await?;
                        s
                    }
                };
                s.write_all(buf).await?;
                Ok(s)
            });
            match res {
                Ok(s) => self.stream = Some(s),
                Err(_) => self.stream = None,
            }
        }
    }
}

impl Drop for AudioSender {
    fn drop(&mut self) {
        self.close_stream();
    }
}

fn run_in_process(
    conn: Connection,
    rt: tokio::runtime::Handle,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    state: Arc<CaptureState>,
) {
    let mut sender = AudioSender::new(conn.clone(), rt, paused);
    while !stop.load(Ordering::Relaxed) && conn.close_reason().is_none() {
        if !state.enabled.load(Ordering::Relaxed) && !state.mute_host.load(Ordering::Relaxed) {
            sender.close_stream();
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let r = CaptureLoop::run(&state, |codec, seq, ch, rate, payload| {
            if stop.load(Ordering::Relaxed) || conn.close_reason().is_some() {
                return false;
            }
            sender.send(codec, seq, ch, rate, payload);
            true
        });
        sender.close_stream();
        if let Err(e) = r
            && !stop.load(Ordering::Relaxed)
        {
            log!("audio capture: {e:#}; retrying in 2s");
            for _ in 0..20 {
                if stop.load(Ordering::Relaxed) || conn.close_reason().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn run_service_agent(
    conn: Connection,
    rt: tokio::runtime::Handle,
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    cfg: Arc<Mutex<AudioCfg>>,
    agent_stdin: Arc<Mutex<Option<File>>>,
) {
    let mut sender = AudioSender::new(conn.clone(), rt, paused);
    while !stop.load(Ordering::Relaxed) && conn.close_reason().is_none() {
        let cur = *cfg.lock().unwrap();
        if !cur.enabled && !cur.mute_host {
            sender.close_stream();
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let exec = match term_proc::self_piped("audio-agent") {
            Ok(e) => e,
            Err(e) => {
                log!("audio agent spawn: {e:#}; retrying in 2s");
                sleep_interruptible(&stop, Duration::from_secs(2));
                continue;
            }
        };

        let term_proc::Exec { proc, mut stdin, mut stdout, stderr } = exec;
        let _ = std::thread::Builder::new().name("audio-agent-err".into()).spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log!("audio agent: {line}");
            }
        });

        let latest = *cfg.lock().unwrap();
        let init_pkt = encode_cmd(CMD_CONFIG, latest);
        if stdin.write_all(&init_pkt).is_err() || stdin.flush().is_err() {
            drop(stdin);
            let _ = proc.wait();
            sleep_interruptible(&stop, Duration::from_secs(2));
            continue;
        }
        *agent_stdin.lock().unwrap() = Some(stdin);

        let mut len_buf = [0u8; 4];
        let mut body = Vec::new();
        while !stop.load(Ordering::Relaxed) && conn.close_reason().is_none() {
            if stdout.read_exact(&mut len_buf).is_err() {
                break;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            if !(8..=65_536).contains(&len) {
                break;
            }
            body.resize(len, 0);
            if stdout.read_exact(&mut body).is_err() {
                break;
            }
            let codec = body[0];
            let seq = u16::from_be_bytes([body[1], body[2]]);
            let ch = body[3].clamp(1, 2);
            let rate = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
            sender.send(codec, seq, ch, rate, &body[8..]);
        }

        sender.close_stream();
        // Close stdout first so an agent blocked in stdout write wakes immediately on BrokenPipe.
        drop(stdout);
        if let Some(mut s) = agent_stdin.lock().unwrap().take() {
            let _ = s.write_all(&encode_cmd(CMD_STOP, AudioCfg::default()));
            let _ = s.flush();
        }
        // Let the agent exit cleanly and drop HostMuteGuard before dropping `proc` (job).
        let proc = Arc::new(proc);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = {
            let proc = proc.clone();
            std::thread::spawn(move || {
                let code = proc.wait();
                let _ = done_tx.send(code);
            })
        };
        let needs_unmute = match done_rx.recv_timeout(Duration::from_millis(1000)) {
            Ok(0) => false,
            Ok(_) => true,
            Err(_) => {
                proc.kill();
                let _ = waiter.join();
                true
            }
        };
        if needs_unmute
            && cfg.lock().unwrap().mute_host
            && term_proc::as_user(true, super::wasapi::force_unmute_default_render).is_err()
        {
            super::wasapi::force_unmute_default_render();
        }

        if !stop.load(Ordering::Relaxed) && conn.close_reason().is_none() {
            sleep_interruptible(&stop, Duration::from_secs(2));
        }
    }
}

fn sleep_interruptible(stop: &AtomicBool, total: Duration) {
    let steps = (total.as_millis() / 100).max(1);
    for _ in 0..steps {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
