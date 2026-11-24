//! Diverts packets matching a WinDivert filter through a simulated
//! bottleneck link (see `link`) and puts them back. Needs Administrator and
//! WinDivert 2.x (`WinDivert.dll` and `WinDivert64.sys` next to the exe).

mod link;

use std::{
    ffi::{CString, c_void},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use link::{Link, LinkConfig};

#[derive(Parser)]
#[command(about = "Bottleneck link emulator on WinDivert, for testing on a LAN")]
struct Cli {
    /// WinDivert filter, e.g. "outbound and udp and ip.DstAddr == 192.168.1.19".
    #[arg(long)]
    filter: String,
    /// Exit after this many seconds.
    #[arg(long, default_value_t = 300.0)]
    seconds: f64,
    #[arg(long)]
    rate_mbps: Option<f64>,
    /// "<mbps>:<secs>,...", cycling; instead of --rate-mbps.
    #[arg(long)]
    schedule: Option<String>,
    #[arg(long, default_value_t = 200.0)]
    queue_ms: f64,
    #[arg(long, default_value_t = 0.0)]
    delay_ms: f64,
    /// Percent of packets that start a loss event.
    #[arg(long, default_value_t = 0.0)]
    loss: f64,
    /// Packets dropped per loss event.
    #[arg(long, default_value_t = 1)]
    loss_burst: u32,
    /// Seconds between hiccups (±20%); 0 for none.
    #[arg(long, default_value_t = 0.0)]
    hiccup_every: f64,
    #[arg(long, default_value_t = 400.0)]
    hiccup_ms: f64,
    #[arg(long, default_value_t = 0.05)]
    hiccup_factor: f64,
    #[arg(long, default_value_t = 1.8)]
    catchup_factor: f64,
    /// Print a line of counters every second.
    #[arg(long)]
    stats: bool,
    /// Print each new flow (protocol, destination) as it is first seen.
    #[arg(long)]
    dump: bool,
    #[arg(long)]
    dll: Option<PathBuf>,
}

#[repr(C, align(8))]
#[derive(Clone, Copy)]
struct Addr([u8; 80]);

type Handle = isize;
const INVALID: Handle = -1;
const MAX_PACKET: usize = 65_535;

struct Api {
    open: unsafe extern "system" fn(*const i8, i32, i16, u64) -> Handle,
    recv: unsafe extern "system" fn(Handle, *mut c_void, u32, *mut u32, *mut Addr) -> i32,
    send: unsafe extern "system" fn(Handle, *const c_void, u32, *mut u32, *const Addr) -> i32,
    shutdown: unsafe extern "system" fn(Handle, i32) -> i32,
    close: unsafe extern "system" fn(Handle) -> i32,
}

impl Api {
    fn load(path: PathBuf) -> Result<Api> {
        let lib = unsafe { libloading::Library::new(&path) }.with_context(|| format!("loading {}", path.display()))?;
        let lib: &'static libloading::Library = Box::leak(Box::new(lib));
        unsafe {
            Ok(Api {
                open: *lib.get(b"WinDivertOpen\0")?,
                recv: *lib.get(b"WinDivertRecv\0")?,
                send: *lib.get(b"WinDivertSend\0")?,
                shutdown: *lib.get(b"WinDivertShutdown\0")?,
                close: *lib.get(b"WinDivertClose\0")?,
            })
        }
    }
}

#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(ms: u32) -> u32;
}

fn schedule(cli: &Cli) -> Result<Vec<(f64, Duration)>> {
    if let Some(s) = &cli.schedule {
        s.split(',')
            .map(|p| {
                let (m, secs) = p.split_once(':').ok_or_else(|| anyhow!("bad schedule step {p:?}"))?;
                Ok((m.trim().parse::<f64>()? * 1e6, Duration::from_secs_f64(secs.trim().parse()?)))
            })
            .collect()
    } else {
        let m = cli.rate_mbps.ok_or_else(|| anyhow!("give --rate-mbps or --schedule"))?;
        Ok(vec![(m * 1e6, Duration::from_secs(1))])
    }
}

/// "udp 192.168.1.19:5000" for an IPv4 or IPv6 packet.
fn flow(p: &[u8]) -> Option<String> {
    let (proto, ip, rest): (u8, std::net::IpAddr, &[u8]) = match p.first()? >> 4 {
        4 if p.len() >= 20 => {
            let ihl = (p[0] & 0xf) as usize * 4;
            (p[9], <[u8; 4]>::try_from(&p[16..20]).ok()?.into(), p.get(ihl..)?)
        }
        6 if p.len() >= 40 => (p[6], <[u8; 16]>::try_from(&p[24..40]).ok()?.into(), &p[40..]),
        _ => return None,
    };
    let name = match proto {
        6 => "tcp",
        17 => "udp",
        1 | 58 => "icmp",
        _ => "other",
    };
    let port = if matches!(proto, 6 | 17) && rest.len() >= 4 { u16::from_be_bytes([rest[2], rest[3]]) } else { 0 };
    Some(format!("{name} {}", std::net::SocketAddr::new(ip, port)))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let steps = schedule(&cli)?;
    if steps.is_empty() || steps.iter().any(|s| s.0 <= 0.0 || s.1.is_zero()) {
        bail!("rates and durations must be positive");
    }
    let cfg = LinkConfig {
        schedule: steps,
        queue: Duration::from_secs_f64(cli.queue_ms / 1000.0),
        delay: Duration::from_secs_f64(cli.delay_ms / 1000.0),
        loss: cli.loss / 100.0 / cli.loss_burst.max(1) as f64,
        loss_burst: cli.loss_burst.max(1),
        hiccup_every: (cli.hiccup_every > 0.0).then(|| Duration::from_secs_f64(cli.hiccup_every)),
        hiccup: Duration::from_secs_f64(cli.hiccup_ms / 1000.0),
        hiccup_factor: cli.hiccup_factor,
        catchup_factor: cli.catchup_factor,
    };
    let dll = match &cli.dll {
        Some(p) => p.clone(),
        None => std::env::current_exe()?.with_file_name("WinDivert.dll"),
    };
    let api = Arc::new(Api::load(dll)?);
    let filter = CString::new(cli.filter.clone())?;
    let h = unsafe { (api.open)(filter.as_ptr(), 0, 0, 0) };
    if h == INVALID {
        let e = std::io::Error::last_os_error();
        let why = match e.raw_os_error() {
            Some(5) => " (run as Administrator)",
            Some(87) => " (bad filter)",
            Some(577) => " (driver signature rejected)",
            Some(2) => " (WinDivert64.sys not next to the DLL)",
            _ => "",
        };
        bail!("WinDivertOpen: {e}{why}");
    }
    unsafe { timeBeginPeriod(1) };

    let start = Instant::now();
    let link = Arc::new(Mutex::new(Link::<(Vec<u8>, Addr)>::new(cfg, start.elapsed().as_nanos() as u64 | 1)));
    let done = Arc::new(AtomicBool::new(false));

    let rx = {
        let (api, link, done, dump) = (api.clone(), link.clone(), done.clone(), cli.dump);
        std::thread::spawn(move || {
            let mut buf = vec![0u8; MAX_PACKET];
            let mut flows = std::collections::HashSet::new();
            while !done.load(Ordering::Relaxed) {
                let mut addr = Addr([0; 80]);
                let mut len = 0u32;
                let ok = unsafe { (api.recv)(h, buf.as_mut_ptr().cast(), buf.len() as u32, &mut len, &mut addr) };
                if ok == 0 {
                    if done.load(Ordering::Relaxed) {
                        break;
                    }
                    eprintln!("WinDivertRecv: {}", std::io::Error::last_os_error());
                    continue;
                }
                let n = len as usize;
                let t = start.elapsed();
                if dump && let Some(f) = flow(&buf[..n]) && flows.insert(f.clone()) {
                    println!("{:6.1}s new flow {f}", t.as_secs_f64());
                }
                link.lock().unwrap().push(t, (buf[..n].to_vec(), addr), n);
            }
        })
    };

    let end = Duration::from_secs_f64(cli.seconds);
    let mut last_stats = Duration::ZERO;
    let mut prev = link::Counters::default();
    loop {
        let t = start.elapsed();
        if t >= end {
            break;
        }
        let ready = link.lock().unwrap().pop_ready(t);
        for (pkt, addr) in ready {
            let mut sent = 0u32;
            if unsafe { (api.send)(h, pkt.as_ptr().cast(), pkt.len() as u32, &mut sent, &addr) } == 0 {
                eprintln!("WinDivertSend: {}", std::io::Error::last_os_error());
            }
        }
        if cli.stats && t - last_stats >= Duration::from_secs(1) {
            let l = link.lock().unwrap();
            let c = l.counters;
            let secs = (t - last_stats).as_secs_f64();
            println!(
                "{:6.1}s rate {:5.1} Mbps{} | in {:4} pkts {:5.1} Mbps, out {:5.1} Mbps | dropped queue {} loss {} | queue {:4.0} ms",
                t.as_secs_f64(),
                l.nominal(t) / 1e6,
                if l.in_hiccup() { " HICCUP" } else { "" },
                c.in_pkts - prev.in_pkts,
                (c.in_bytes - prev.in_bytes) as f64 * 8.0 / secs / 1e6,
                (c.out_bytes - prev.out_bytes) as f64 * 8.0 / secs / 1e6,
                c.dropped_queue - prev.dropped_queue,
                c.dropped_loss - prev.dropped_loss,
                l.queue_ms(),
            );
            prev = c;
            last_stats = t;
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    done.store(true, Ordering::Relaxed);
    unsafe { (api.shutdown)(h, 3) };
    let _ = rx.join();
    unsafe { (api.close)(h) };
    Ok(())
}
