//! The remote terminal: an interactive shell, one-shot commands and file
//! copies on a computer this one may control, on a connection of its own
//! (ALPN `rc/term/1`). Who may is decided as for a desktop session
//! (`node::admit`); the processes are in `term_proc`.
//!
//! One stream each way of frames `[kind u8][length u32 BE][payload]`. The
//! client opens with `OPEN`; the host may answer `WAITING` (asking its user),
//! then `READY`, or closes the connection with a `net::CLOSE_*` code.

use std::{
    ffi::c_void,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Result, anyhow, bail};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::mpsc::{Receiver, Sender};

use crate::{
    net, node,
    term_proc::{self, Pty},
};

pub const ALPN: &[u8] = b"rc/term/1";

pub const EXEC_HELP: &str = "\
命令在对方的 PowerShell 里运行（装了 PowerShell 7 用 pwsh，否则 Windows PowerShell 5.1），
输出是 UTF-8。stdout、stderr 分开带回，本命令的退出码就是对方命令的退出码：
exit N 给 N；最后一条命令失败时是那个程序的退出码或 1；否则 0。
连不上、被拒绝等本机这边的问题，退出码 255，原因写在 stderr。
本机 stdin 不是终端时（管道、重定向），会转给对方命令的 stdin。
权限和控制屏幕一样：对方是「我的电脑」直接运行，信任设备要对方点一次允许；
对方装了后台服务时以管理员身份运行。

例子：
  remote-control exec laptop \"Get-ComputerInfo | Select-Object OsName, OsVersion\"
  remote-control exec laptop --cwd D:\\proj \"git status\"
  Get-Content a.ps1 | remote-control exec laptop \"$input | Out-String | Invoke-Expression\"";

pub const CP_HELP: &str = "\
一边是本机路径，一边是 设备:路径（C:\\… 这种单个字母加冒号是本机盘符）。
对方的相对路径从对方用户的主文件夹算起，~ 就是那个文件夹。
目标是已有的文件夹时复制到它里面；否则复制成这个名字。文件夹连同里面的东西一起复制，已有的同名文件会被覆盖。

例子：
  remote-control cp .\\report.pdf laptop:Desktop
  remote-control cp laptop:D:\\logs .\\laptop-logs";

pub const OPEN: u8 = 1;
pub const WAITING: u8 = 2;
pub const READY: u8 = 3;
/// Client -> host: keys for the shell, stdin for a command.
pub const INPUT: u8 = 10;
pub const INPUT_END: u8 = 11;
pub const RESIZE: u8 = 12;
/// Host -> client: the shell's output, a command's stdout.
pub const OUTPUT: u8 = 20;
pub const STDERR: u8 = 21;
pub const EXIT: u8 = 22;
/// File copies, either way: a file (path, size) and its data, a folder (path).
pub const FILE: u8 = 30;
pub const DATA: u8 = 31;
pub const DIR: u8 = 32;
pub const END: u8 = 33;
pub const ERROR: u8 = 34;
pub const DONE: u8 = 35;

const MAX_FRAME: usize = 1 << 20;
const CHUNK: usize = 64 * 1024;

pub type Frame = (u8, Vec<u8>);

pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(kind);
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// The next frame, or None at the end of the stream.
pub async fn read_frame(recv: &mut RecvStream) -> Result<Option<Frame>> {
    let mut head = [0u8; 5];
    match recv.read_exact(&mut head).await {
        Ok(()) => {}
        Err(iroh::endpoint::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        bail!("frame of {len} bytes is too large");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    Ok(Some((head[0], buf)))
}

pub async fn write_frame(send: &mut SendStream, kind: u8, payload: &[u8]) -> Result<()> {
    send.write_all(&frame(kind, payload)).await?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    Shell,
    Exec,
    /// A file or folder to the host.
    Put,
    /// A file or folder from the host.
    Get,
}

/// The client's first frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    /// The device proof for the client's session key (see `ident`).
    pub proof: String,
    pub what: What,
    pub cols: u16,
    pub rows: u16,
    /// The command, or the path on the host.
    pub text: String,
    /// Where the command runs (empty: the user's profile folder).
    pub cwd: String,
}

fn put_str(v: &mut Vec<u8>, s: &str) {
    v.extend_from_slice(&(s.len() as u32).to_be_bytes());
    v.extend_from_slice(s.as_bytes());
}

/// Reads the fields of a payload in order.
pub struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Fields(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            bail!("frame too short");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }

    pub fn str(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        Ok(String::from_utf8(self.take(n)?.to_vec())?)
    }
}

impl Open {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::new();
        put_str(&mut v, &self.proof);
        v.push(match self.what {
            What::Shell => 1,
            What::Exec => 2,
            What::Put => 3,
            What::Get => 4,
        });
        v.extend_from_slice(&self.cols.to_be_bytes());
        v.extend_from_slice(&self.rows.to_be_bytes());
        put_str(&mut v, &self.text);
        put_str(&mut v, &self.cwd);
        v
    }

    pub fn decode(b: &[u8]) -> Result<Open> {
        let mut f = Fields::new(b);
        let proof = f.str()?;
        let what = match f.take(1)?[0] {
            1 => What::Shell,
            2 => What::Exec,
            3 => What::Put,
            4 => What::Get,
            w => bail!("unknown request {w}"),
        };
        Ok(Open { proof, what, cols: f.u16()?, rows: f.u16()?, text: f.str()?, cwd: f.str()? })
    }
}

pub fn resize(cols: u16, rows: u16) -> Vec<u8> {
    [cols.to_be_bytes(), rows.to_be_bytes()].concat()
}

/// Sends `src` (a file, or a folder with everything in it) as frames, paths
/// relative to its parent with `/` between, then `END`. Blocking: runs on a
/// thread of its own (the host's impersonates its user there). The number of files.
pub fn send_tree(src: &Path, out: &Sender<Frame>) -> Result<u64> {
    const MAX_DEPTH: usize = 64;
    let name = src.file_name().ok_or_else(|| anyhow!("{} 不是文件或文件夹", src.display()))?.to_string_lossy().into_owned();
    let send = |kind: u8, payload: Vec<u8>| out.blocking_send((kind, payload)).map_err(|_| anyhow!("连接断开了"));
    let mut files = 0u64;
    fn walk(path: &Path, rel: &str, depth: usize, send: &dyn Fn(u8, Vec<u8>) -> Result<()>, files: &mut u64) -> Result<()> {
        if depth > MAX_DEPTH {
            bail!("{} 目录层级超过 {MAX_DEPTH} 层", path.display());
        }
        let meta = if depth == 0 {
            std::fs::metadata(path)
        } else {
            std::fs::symlink_metadata(path)
        }
        .map_err(|e| anyhow!("{}：{e}", path.display()))?;
        if depth > 0 && meta.file_type().is_symlink() {
            return Ok(());
        }
        let mut head = Vec::new();
        put_str(&mut head, rel);
        if meta.is_dir() {
            send(DIR, head)?;
            let mut entries: Vec<_> = std::fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                walk(&e.path(), &format!("{rel}/{}", e.file_name().to_string_lossy()), depth + 1, send, files)?;
            }
            return Ok(());
        }
        let mut f = std::fs::File::open(path).map_err(|e| anyhow!("{}：{e}", path.display()))?;
        head.extend_from_slice(&meta.len().to_be_bytes());
        send(FILE, head)?;
        let mut left = meta.len();
        let mut buf = vec![0u8; CHUNK];
        while left > 0 {
            let n = f.read(&mut buf[..CHUNK.min(left as usize)])?;
            if n == 0 {
                bail!("{} 在复制时变短了", path.display());
            }
            send(DATA, buf[..n].to_vec())?;
            left -= n as u64;
        }
        *files += 1;
        Ok(())
    }
    walk(src, &name, 0, &send, &mut files)?;
    send(END, Vec::new())?;
    Ok(files)
}

/// Where a sent path goes: into `target` when it is a folder, else to `target` itself.
fn dest(target: &Path, into_dir: bool, rel: &str) -> Result<PathBuf> {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.iter().any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains([':', '\\'])) {
        bail!("bad path {rel:?}");
    }
    let parts = if into_dir { &parts[..] } else { &parts[1..] };
    Ok(parts.iter().fold(target.to_path_buf(), |p, c| p.join(c)))
}

/// Writes what `send_tree` sent to `target`; (files, bytes).
pub fn recv_tree(target: &Path, inp: &mut Receiver<Frame>) -> Result<(u64, u64)> {
    let into_dir = target.is_dir();
    let (mut files, mut bytes) = (0u64, 0u64);
    let next = |inp: &mut Receiver<Frame>| inp.blocking_recv().ok_or_else(|| anyhow!("连接断开了"));
    loop {
        let (kind, payload) = next(inp)?;
        let mut f = Fields::new(&payload);
        match kind {
            DIR => {
                let d = dest(target, into_dir, &f.str()?)?;
                std::fs::create_dir_all(&d).map_err(|e| anyhow!("{}：{e}", d.display()))?;
            }
            FILE => {
                let d = dest(target, into_dir, &f.str()?)?;
                let size = f.u64()?;
                let mut out = std::fs::File::create(&d).map_err(|e| anyhow!("{}：{e}", d.display()))?;
                let mut left = size;
                while left > 0 {
                    let (k, data) = next(inp)?;
                    if k != DATA || data.len() as u64 > left {
                        bail!("file data out of order");
                    }
                    out.write_all(&data)?;
                    left -= data.len() as u64;
                }
                files += 1;
                bytes += size;
            }
            END => return Ok((files, bytes)),
            ERROR => bail!("{}", String::from_utf8_lossy(&payload)),
            k => bail!("unexpected frame {k}"),
        }
    }
}

/// Writes the frames from the returned sender to `send`, in order; finishes the stream when all senders are gone.
fn writer(mut send: SendStream) -> Sender<Frame> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(64);
    tokio::spawn(async move {
        while let Some((k, p)) = rx.recv().await {
            if write_frame(&mut send, k, &p).await.is_err() {
                return;
            }
        }
        let _ = send.finish();
        let _ = send.stopped().await;
    });
    tx
}

/// Copies a pipe to frames of `kind` on a thread; ends at the pipe's end.
/// When `cancel` is set (or the receiver closes), drops `out` so the writer
/// task exits and continues draining `from` to EOF so `ClosePseudoConsole` never deadlocks.
fn pump(mut from: std::fs::File, kind: u8, out: Sender<Frame>, cancel: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut out = Some(out);
        let mut buf = vec![0u8; CHUNK];
        while let Ok(n) = from.read(&mut buf) {
            if n == 0 {
                break;
            }
            if cancel.load(Ordering::Relaxed) {
                out = None;
                continue;
            }
            if let Some(tx) = out.as_ref() {
                let mut frame = Some((kind, buf[..n].to_vec()));
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        out = None;
                        break;
                    }
                    let f = frame.take().unwrap();
                    match tx.try_send(f) {
                        Ok(()) => break,
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            out = None;
                            break;
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Full(f)) => {
                            frame = Some(f);
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                    }
                }
            }
        }
    })
}

/// Serves an admitted request until it is done or the client goes.
pub async fn serve(open: Open, conn: Connection, send: SendStream, recv: RecvStream, service: bool) -> Result<()> {
    let out = writer(send);
    out.send((READY, Vec::new())).await?;
    match open.what {
        What::Shell => serve_shell(open, conn, out, recv, service).await,
        What::Exec => serve_exec(open, conn, out, recv, service).await,
        What::Put => serve_put(open, out, recv, service).await,
        What::Get => serve_get(open, out, service).await,
    }
}

fn exit_frame(code: u32) -> Frame {
    (EXIT, code.to_be_bytes().to_vec())
}

async fn serve_shell(open: Open, conn: Connection, out: Sender<Frame>, mut recv: RecvStream, service: bool) -> Result<()> {
    let (pty, io) = tokio::task::spawn_blocking(move || Pty::spawn(service, open.cols, open.rows)).await??;
    let pty = Arc::new(pty);
    let cancel = Arc::new(AtomicBool::new(false));
    let reader = pump(io.output, OUTPUT, out.clone(), cancel.clone());
    let (keys, keys_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut input = io.input;
    std::thread::spawn(move || {
        for k in keys_rx {
            if input.write_all(&k).is_err() {
                break;
            }
        }
    });
    let mut waiter = {
        let pty = pty.clone();
        tokio::task::spawn_blocking(move || pty.wait())
    };
    loop {
        tokio::select! {
            code = &mut waiter => {
                let _ = tokio::task::spawn_blocking(move || reader.join()).await;
                out.send(exit_frame(code.unwrap_or(1))).await?;
                return Ok(());
            }
            _ = conn.closed() => {
                cancel.store(true, Ordering::Relaxed);
                drop(out);
                pty.kill();
                let _ = waiter.await;
                let _ = tokio::task::spawn_blocking(move || reader.join()).await;
                return Ok(());
            }
            f = read_frame(&mut recv) => match f {
                Ok(Some((INPUT, b))) => {
                    let _ = keys.send(b);
                }
                Ok(Some((RESIZE, b))) => {
                    let mut f = Fields::new(&b);
                    if let (Ok(c), Ok(r)) = (f.u16(), f.u16()) {
                        let _ = pty.resize(c, r);
                    }
                }
                Ok(Some(_)) => {}
                _ => {
                    cancel.store(true, Ordering::Relaxed);
                    drop(out);
                    pty.kill();
                    let _ = waiter.await;
                    let _ = tokio::task::spawn_blocking(move || reader.join()).await;
                    return Ok(());
                }
            }
        }
    }
}

async fn serve_exec(open: Open, conn: Connection, out: Sender<Frame>, mut recv: RecvStream, service: bool) -> Result<()> {
    let cwd = (!open.cwd.is_empty()).then(|| PathBuf::from(&open.cwd));
    let text = open.text.clone();
    let e = tokio::task::spawn_blocking(move || term_proc::exec(service, &text, cwd.as_deref())).await??;
    let proc = Arc::new(e.proc);
    let cancel = Arc::new(AtomicBool::new(false));
    let pumps = [pump(e.stdout, OUTPUT, out.clone(), cancel.clone()), pump(e.stderr, STDERR, out.clone(), cancel.clone())];
    let (keys, keys_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut stdin = e.stdin;
    std::thread::spawn(move || {
        for k in keys_rx {
            if stdin.write_all(&k).is_err() {
                break;
            }
        }
    });
    let mut keys = Some(keys);
    let mut waiter = {
        let proc = proc.clone();
        tokio::task::spawn_blocking(move || proc.wait())
    };
    let mut client_done = false;
    loop {
        tokio::select! {
            code = &mut waiter => {
                let _ = tokio::task::spawn_blocking(move || pumps.map(|p| p.join())).await;
                out.send(exit_frame(code.unwrap_or(1))).await?;
                return Ok(());
            }
            _ = conn.closed() => {
                cancel.store(true, Ordering::Relaxed);
                drop(out);
                proc.kill();
                let _ = waiter.await;
                let _ = tokio::task::spawn_blocking(move || pumps.map(|p| p.join())).await;
                return Ok(());
            }
            f = read_frame(&mut recv), if !client_done => match f {
                Ok(Some((INPUT, b))) => {
                    if let Some(k) = &keys {
                        let _ = k.send(b);
                    }
                }
                Ok(Some((INPUT_END, _))) => keys = None,
                Ok(Some(_)) => {}
                Ok(None) => client_done = true,
                Err(_) => {
                    cancel.store(true, Ordering::Relaxed);
                    drop(out);
                    proc.kill();
                    let _ = waiter.await;
                    let _ = tokio::task::spawn_blocking(move || pumps.map(|p| p.join())).await;
                    return Ok(());
                }
            }
        }
    }
}

/// A path on the host: absolute as given, else from the user's profile folder (`~` is that folder).
fn resolve(text: &str, service: bool) -> Result<PathBuf> {
    let p = Path::new(text);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let rest = text.strip_prefix('~').unwrap_or(text).trim_start_matches(['/', '\\']);
    Ok(term_proc::home(service)?.join(rest))
}

async fn serve_put(open: Open, out: Sender<Frame>, mut recv: RecvStream, service: bool) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(64);
    let job = tokio::task::spawn_blocking(move || {
        // Resolved before impersonating: as the user, the console user's token cannot be read.
        let path = resolve(&open.text, service)?;
        term_proc::as_user(service, || recv_tree(&path, &mut rx)).and_then(|r| r)
    });
    while let Ok(Some(f)) = read_frame(&mut recv).await {
        let end = f.0 == END;
        if tx.send(f).await.is_err() || end {
            break;
        }
    }
    drop(tx);
    match job.await? {
        Ok((files, bytes)) => out.send((DONE, [files.to_be_bytes(), bytes.to_be_bytes()].concat())).await?,
        Err(e) => out.send((ERROR, format!("{e:#}").into_bytes())).await?,
    }
    Ok(())
}

async fn serve_get(open: Open, out: Sender<Frame>, service: bool) -> Result<()> {
    let tx = out.clone();
    let r = tokio::task::spawn_blocking(move || {
        let path = resolve(&open.text, service)?;
        term_proc::as_user(service, || send_tree(&path, &tx)).and_then(|r| r)
    })
    .await?;
    if let Err(e) = r {
        out.send((ERROR, format!("{e:#}").into_bytes())).await?;
    }
    Ok(())
}

/// One request to the running node; its answer without the `ok`.
fn ask(req: &str) -> Result<String> {
    let a = node::call(req).map_err(|_| anyhow!("remote-control 没在运行：先打开大厅窗口（或装后台服务）"))?;
    match a.split_once('\t') {
        Some(("ok", rest)) => Ok(rest.to_string()),
        Some((_, e)) => bail!("{e}"),
        None => bail!("unexpected answer {a:?}"),
    }
}

/// A device the lobby shows: (id, name, status).
pub fn devices() -> Result<Vec<(iroh::EndpointId, String, String)>> {
    let snap = ask("peek")?;
    Ok(snap
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.first() == Some(&"dev") && f.len() >= 10).then(|| (f[1].parse().ok(), f[9].to_string(), f[5].to_string()))
        })
        .filter_map(|(id, name, status)| Some((id?, name, status)))
        .collect())
}

/// The device `query` names: its id or the start of it, or its name (whole, else a unique part).
pub fn find(query: &str) -> Result<(iroh::EndpointId, String)> {
    let all = devices()?;
    let q = query.trim().to_lowercase();
    if let Ok(id) = q.parse::<iroh::EndpointId>()
        && !all.iter().any(|d| d.0 == id)
    {
        return Ok((id, id.fmt_short().to_string()));
    }
    let by = |f: &dyn Fn(&(iroh::EndpointId, String, String)) -> bool| -> Vec<(iroh::EndpointId, String)> {
        all.iter().filter(|d| f(d)).map(|d| (d.0, d.1.clone())).collect()
    };
    for hits in [
        by(&|d| d.0.to_string() == q),
        by(&|d| d.1.to_lowercase() == q),
        if q.len() >= 4 { by(&|d| d.0.to_string().starts_with(&q)) } else { Vec::new() },
        by(&|d| d.1.to_lowercase().contains(&q)),
    ] {
        match hits.len() {
            0 => continue,
            1 => return Ok(hits[0].clone()),
            _ => bail!("「{query}」对上了好几台：{}；写全名或设备码", hits.iter().map(|h| h.1.as_str()).collect::<Vec<_>>().join("、")),
        }
    }
    let names: Vec<&str> = all.iter().map(|d| d.1.as_str()).collect();
    bail!("找不到「{query}」。大厅里有：{}", if names.is_empty() { "（没有设备）".to_string() } else { names.join("、") })
}

/// A request the host has admitted.
pub struct Session {
    pub ep: iroh::Endpoint,
    pub conn: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
}

/// Ends the connection so the host learns at once, not at the idle timeout.
async fn bye(conn: &Connection, ep: &iroh::Endpoint) {
    conn.close(0u32.into(), b"bye");
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), ep.close()).await;
}

/// The connection the console handler ends.
static ON_EXIT: OnceLock<(Connection, iroh::Endpoint, tokio::runtime::Handle)> = OnceLock::new();

/// Closing the terminal window (or Ctrl+C on a command, signing out) ends the
/// process without unwinding, so `bye` never runs and the host keeps the
/// session, and its bar, until the idle timeout. The handler says goodbye
/// first, then lets Windows end the process as before.
fn bye_on_console_exit(conn: &Connection, ep: &iroh::Endpoint) {
    unsafe extern "system" fn on_console(_ctrl: u32) -> windows::core::BOOL {
        if let Some((conn, ep, rt)) = ON_EXIT.get() {
            rt.block_on(bye(conn, ep));
        }
        false.into()
    }
    if ON_EXIT.set((conn.clone(), ep.clone(), tokio::runtime::Handle::current())).is_ok() {
        unsafe {
            let _ = windows::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console), true);
        }
    }
}

/// Connects to `id` with a session key of its own (and the node's proof for it), as a session window does, and opens `open`.
pub async fn connect(id: iroh::EndpointId, mut open: Open) -> Result<Session> {
    use std::time::Duration;
    let dial = ask(&format!("dial\t{id}"))?;
    let f: Vec<&str> = dial.split('\t').collect();
    let list = |i: usize| -> Vec<&str> { f.get(i).map_or(Vec::new(), |s| s.split(',').filter(|x| !x.is_empty()).collect()) };
    let mut addrs: Vec<std::net::SocketAddr> = list(0).iter().filter_map(|a| a.parse().ok()).collect();
    // For tests: a direct address the node does not know.
    if let Ok(a) = std::env::var("RC_TEST_ADDR")
        && let Ok(a) = a.parse()
    {
        addrs.insert(0, a);
    }
    let via: Vec<iroh::RelayUrl> = list(1).iter().filter_map(|a| a.parse().ok()).collect();
    let own: Vec<iroh::RelayUrl> = list(2).iter().filter_map(|a| a.parse().ok()).collect();
    let public = f.get(3) != Some(&"0");
    let key = iroh::SecretKey::generate();
    let proof = ask(&format!("proof\t{}", key.public()))?;
    let relays = net::Relays { own: own.clone(), public, theirs: via.clone(), token: Some(proof.clone()) };
    let ep = net::endpoint(key, &relays, true).await?;
    let mut via = via;
    if via.is_empty() {
        via = own;
        if public && via.is_empty() {
            via.extend(net::public_relays());
        }
    }
    let target = net::dial_addr(id, &addrs, &via);
    let conn = match tokio::time::timeout(Duration::from_secs(15), ep.connect(target, ALPN)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) if format!("{e:#}").contains("no application protocol") || format!("{e:#}").contains("ALPN") => {
            bail!("对方的 remote-control 版本太旧，没有终端功能，请对方更新")
        }
        Ok(Err(e)) if !format!("{e:#}").contains("timed out") => bail!("连不上对方：{e:#}"),
        _ => bail!("连不上对方：没有回应（对方没开机、没联网、睡着了，或者找不到通路）"),
    };
    bye_on_console_exit(&conn, &ep);
    let refused =|conn: &Connection, e: &dyn std::fmt::Display| match conn.close_reason() {
        Some(iroh::endpoint::ConnectionError::ApplicationClosed(c)) => anyhow!("{}", crate::view::close_text(c.error_code.into_inner() as u32)),
        _ => anyhow!("连接断开：{e}"),
    };
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| refused(&conn, &e))?;
    open.proof = proof;
    write_frame(&mut send, OPEN, &open.encode()).await.map_err(|e| refused(&conn, &e))?;
    loop {
        match read_frame(&mut recv).await {
            Ok(Some((READY, _))) => return Ok(Session { ep, conn, send, recv }),
            Ok(Some((WAITING, _))) => eprintln!("等对方在弹窗里点【允许控制】……"),
            Ok(Some(_)) => {}
            Ok(None) => return Err(refused(&conn, &"对方结束了连接")),
            Err(e) => return Err(refused(&conn, &e)),
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    crate::stats::QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

/// `remote-control exec`: runs one command there; our exit code is its exit code.
pub fn exec(device: &str, command: &str, cwd: Option<&str>) -> Result<i32> {
    use std::io::IsTerminal;
    let rt = runtime()?;
    rt.block_on(async {
        let (id, _) = find(device)?;
        let open = Open { proof: String::new(), what: What::Exec, cols: 0, rows: 0, text: command.to_string(), cwd: cwd.unwrap_or("").to_string() };
        let Session { ep, conn, mut send, mut recv } = connect(id, open).await?;
        if std::io::stdin().is_terminal() {
            write_frame(&mut send, INPUT_END, &[]).await?;
        } else {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
            std::thread::spawn(move || {
                let mut buf = vec![0u8; CHUNK];
                let mut stdin = std::io::stdin().lock();
                while let Ok(n) = stdin.read(&mut buf) {
                    if n == 0 || tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });
            tokio::spawn(async move {
                while let Some(b) = rx.recv().await {
                    if write_frame(&mut send, INPUT, &b).await.is_err() {
                        return;
                    }
                }
                let _ = write_frame(&mut send, INPUT_END, &[]).await;
                let _ = send.finish();
            });
        }
        let code = loop {
            match read_frame(&mut recv).await {
                Ok(Some((OUTPUT, b))) => {
                    let mut o = std::io::stdout().lock();
                    o.write_all(&b)?;
                    o.flush()?;
                }
                Ok(Some((STDERR, b))) => std::io::stderr().lock().write_all(&b)?,
                Ok(Some((EXIT, b))) => break Fields::new(&b).u32()? as i32,
                Ok(Some(_)) => {}
                _ => bail!("连接断开了，命令可能还没跑完"),
            }
        };
        bye(&conn, &ep).await;
        Ok(code)
    })
}

/// `设备:路径` → (device, path); None for a local path (`C:\…` is a drive letter, not a device).
fn remote_side(s: &str) -> Option<(&str, &str)> {
    let (d, p) = s.split_once(':')?;
    (d.chars().count() > 1).then_some((d, p))
}

/// `remote-control cp`: one side is `设备:路径`. A folder goes with everything in it.
pub fn cp(from: &str, to: &str) -> Result<()> {
    let (what, device, remote, local) = match (remote_side(from), remote_side(to)) {
        (None, Some((d, p))) => (What::Put, d, p, from),
        (Some((d, p)), None) => (What::Get, d, p, to),
        (Some(_), Some(_)) => bail!("两边都是远程路径；一次只能在本机和一台设备之间复制"),
        (None, None) => bail!("有一边要写成 设备:路径，例如 laptop:D:\\Desktop\\a.txt"),
    };
    let local = PathBuf::from(local);
    let rt = runtime()?;
    let (files, bytes) = rt.block_on(async {
        let (id, _) = find(device)?;
        let open = Open { proof: String::new(), what, cols: 0, rows: 0, text: remote.to_string(), cwd: String::new() };
        let Session { ep, conn, mut send, mut recv } = connect(id, open).await?;
        let r = match what {
            What::Put => {
                let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(64);
                let sender = tokio::task::spawn_blocking(move || send_tree(&local, &tx));
                while let Some((k, p)) = rx.recv().await {
                    write_frame(&mut send, k, &p).await?;
                }
                sender.await??;
                match read_frame(&mut recv).await {
                    Ok(Some((DONE, b))) => {
                        let mut f = Fields::new(&b);
                        Ok((f.u64()?, f.u64()?))
                    }
                    Ok(Some((ERROR, b))) => Err(anyhow!("{}", String::from_utf8_lossy(&b))),
                    _ => Err(anyhow!("连接断开了，可能没复制完")),
                }
            }
            _ => {
                let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(64);
                let writer = tokio::task::spawn_blocking(move || recv_tree(&local, &mut rx));
                while let Ok(Some(f)) = read_frame(&mut recv).await {
                    let end = matches!(f.0, END | ERROR);
                    if tx.send(f).await.is_err() || end {
                        break;
                    }
                }
                drop(tx);
                writer.await?
            }
        };
        bye(&conn, &ep).await;
        r
    })?;
    eprintln!("已复制 {files} 个文件，共 {}", size_text(bytes));
    Ok(())
}

fn size_text(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} 字节"),
    }
}

/// This console in raw VT mode while the shell runs; the old modes come back when dropped.
struct RawConsole {
    input: windows::Win32::Foundation::HANDLE,
    output: windows::Win32::Foundation::HANDLE,
    modes: (u32, u32),
    cp: (u32, u32),
}

impl RawConsole {
    fn new() -> Result<RawConsole> {
        use windows::Win32::System::Console::*;
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE)?;
            let output = GetStdHandle(STD_OUTPUT_HANDLE)?;
            let (mut mi, mut mo) = (CONSOLE_MODE(0), CONSOLE_MODE(0));
            GetConsoleMode(input, &mut mi).map_err(|_| anyhow!("shell 要在终端窗口里用；给程序调用请用 exec"))?;
            GetConsoleMode(output, &mut mo)?;
            let me = RawConsole { input, output, modes: (mi.0, mo.0), cp: (GetConsoleCP(), GetConsoleOutputCP()) };
            SetConsoleMode(input, ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_WINDOW_INPUT)?;
            SetConsoleMode(output, mo | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN)?;
            let _ = SetConsoleCP(65001);
            let _ = SetConsoleOutputCP(65001);
            Ok(me)
        }
    }

    /// Columns and rows of the visible window.
    fn size(&self) -> (u16, u16) {
        use windows::Win32::System::Console::*;
        let mut i = CONSOLE_SCREEN_BUFFER_INFO::default();
        if unsafe { GetConsoleScreenBufferInfo(self.output, &mut i) }.is_err() {
            return (120, 30);
        }
        ((i.srWindow.Right - i.srWindow.Left + 1).max(1) as u16, (i.srWindow.Bottom - i.srWindow.Top + 1).max(1) as u16)
    }
}

impl Drop for RawConsole {
    fn drop(&mut self) {
        use windows::Win32::System::Console::*;
        unsafe {
            let _ = SetConsoleMode(self.input, CONSOLE_MODE(self.modes.0));
            let _ = SetConsoleMode(self.output, CONSOLE_MODE(self.modes.1));
            let _ = SetConsoleCP(self.cp.0);
            let _ = SetConsoleOutputCP(self.cp.1);
        }
    }
}

/// Keys typed here, as UTF-8, on a thread of their own.
fn read_keys(input: windows::Win32::Foundation::HANDLE, tx: Sender<Vec<u8>>) {
    let raw = input.0 as isize;
    std::thread::spawn(move || {
        use windows::Win32::System::Console::ReadConsoleW;
        let input = windows::Win32::Foundation::HANDLE(raw as *mut c_void);
        let mut buf = [0u16; 512];
        let mut carry: Option<u16> = None;
        loop {
            let mut n = 0u32;
            if unsafe { ReadConsoleW(input, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut n, None) }.is_err() || n == 0 {
                return;
            }
            let mut units: Vec<u16> = carry.take().into_iter().collect();
            units.extend_from_slice(&buf[..n as usize]);
            if units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
                carry = units.pop();
            }
            if tx.blocking_send(String::from_utf16_lossy(&units).into_bytes()).is_err() {
                return;
            }
        }
    });
}

/// Writes the shell's output; what is not UTF-8 shows as replacement characters.
fn show(carry: &mut Vec<u8>, b: &[u8]) {
    carry.extend_from_slice(b);
    let keep = match std::str::from_utf8(carry) {
        Err(e) if e.error_len().is_none() => carry.len() - e.valid_up_to(),
        _ => 0,
    };
    let done: Vec<u8> = carry.drain(..carry.len() - keep).collect();
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(String::from_utf8_lossy(&done).as_bytes());
    let _ = o.flush();
}

/// The skill that teaches Claude Code and Codex the commands; `{exe}` is this program.
const SKILL: &str = r#"---
name: remote-control
description: Run commands on, inspect, fix, and copy files to or from the user's other Windows computers (the devices in the remote-control lobby, e.g. a laptop or a family member's PC). Use when the user asks to do anything on another computer.
---

# 操作别的电脑（remote-control）

程序：`{exe}`（下面写成 `remote-control`）。它连的是本机 remote-control 大厅里的设备，权限和控制屏幕一样。

## 先看有哪些设备

```
remote-control devices
```

每行：名字、状态（online / offline …）、设备码。命令里的「设备」写名字（不分大小写，写一部分也行，只要不重名）或设备码。

## 运行命令

```
remote-control exec <设备> '<PowerShell 命令>'
remote-control exec <设备> --cwd 'D:\proj' 'git status'
```

- 命令在对方的 PowerShell 里跑（有 PowerShell 7 用 pwsh），输出 UTF-8，stdout 和 stderr 分开带回。
- 退出码就是对方命令的退出码；255 是本机这边的问题（连不上、被拒绝、对方没开机），原因在 stderr。
- 每次 exec 都是一个新的 PowerShell 进程，变量和当前目录不会留到下一次；要在某个目录里做事，用 --cwd 或在同一条命令里先 Set-Location。默认目录是对方用户的主文件夹。
- 没有终端：别跑要人输入的程序（会一直等）。用 -y、-Force、-NonInteractive 之类的参数，或者把输入从 stdin 管道给它：`Get-Content in.txt | remote-control exec <设备> '$input | ...'`。
- 在 bash 里用单引号包住整条命令，免得 `$` 被 bash 展开；在 PowerShell 里也用单引号。
- 第一次连信任设备时，对方屏幕上会弹窗，要对方点【允许控制】（最多等 30 秒，stderr 会提示）。之后 60 秒内的命令不再问。对方是「我的电脑」时直接运行。对方装了后台服务时以管理员身份运行。
- 对方屏幕顶部会显示「本机名字正在控制」，对方可以随时点【断开】。

## 复制文件

```
remote-control cp .\report.pdf <设备>:Desktop
remote-control cp '<设备>:D:\logs' .\logs
```

一边写成 `设备:路径`。对方的相对路径从对方用户的主文件夹算起。目标是已有文件夹时放进去，否则用这个名字；文件夹整个复制，同名文件会被覆盖。

## 习惯

- 动手改对方的系统前，先用只读命令看清楚情况，把要做的事告诉用户。
- 删除、重启、改网络和代理这类难以撤回的操作，先问用户。
- 用户想自己操作时，告诉用户在大厅设备的 ⋯ 菜单或控制窗口标题栏点【终端】。
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillState {
    NotInstalled,
    Installed,
    Outdated,
}

fn skill_home() -> Result<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from).ok_or_else(|| anyhow!("找不到用户文件夹"))
}

fn skill_dirs(home: &Path) -> [(&'static str, PathBuf); 2] {
    [".claude", ".agents"].map(|dir| (dir, home.join(dir).join("skills").join("remote-control")))
}

fn rendered_skill(exe: &Path) -> String {
    SKILL.replace("{exe}", &exe.display().to_string())
}

fn install_skill_in(home: &Path, exe: &Path) -> Result<String> {
    let text = rendered_skill(exe);
    let mut done = Vec::new();
    for (dir, d) in skill_dirs(home) {
        std::fs::create_dir_all(&d).map_err(|e| anyhow!("{}：{e}", d.display()))?;
        std::fs::write(d.join("SKILL.md"), &text).map_err(|e| anyhow!("{}：{e}", d.display()))?;
        done.push(format!("~\\{dir}\\skills"));
    }
    Ok(done.join(" 和 "))
}

/// Writes the skill for Claude Code (`~/.claude/skills`) and Codex (`~/.agents/skills`); where it went.
pub fn install_skill() -> Result<String> {
    let home = skill_home()?;
    let exe = std::env::current_exe()?;
    install_skill_in(&home, &exe)
}

fn skill_state_in(home: &Path, exe: &Path) -> SkillState {
    let expected = rendered_skill(exe);
    let mut any_exists = false;
    let mut all_match = true;
    for (_, d) in skill_dirs(home) {
        let file = d.join("SKILL.md");
        match std::fs::read_to_string(&file) {
            Ok(content) => {
                any_exists = true;
                if content != expected {
                    all_match = false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                all_match = false;
            }
            Err(_) => {
                if file.exists() {
                    any_exists = true;
                }
                all_match = false;
            }
        }
    }
    if !any_exists {
        SkillState::NotInstalled
    } else if all_match {
        SkillState::Installed
    } else {
        SkillState::Outdated
    }
}

/// Checks whether the remote-control skill is installed for both Claude Code
/// and Codex and matches this executable's path and current template.
pub fn skill_state() -> SkillState {
    let Ok(home) = skill_home() else { return SkillState::NotInstalled };
    let Ok(exe) = std::env::current_exe() else { return SkillState::NotInstalled };
    skill_state_in(&home, &exe)
}

fn uninstall_skill_in(home: &Path) -> Result<()> {
    for (_, d) in skill_dirs(home) {
        match std::fs::remove_file(d.join("SKILL.md")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow!("{}：{e}", d.display())),
        }
        let _ = std::fs::remove_dir(&d);
    }
    Ok(())
}

/// Removes the installed remote-control skill from Claude Code and Codex.
pub fn uninstall_skill() -> Result<()> {
    let home = skill_home()?;
    uninstall_skill_in(&home)
}

/// The device of this session window, for its 【终端】 button.
static SESSION: std::sync::Mutex<Option<(iroh::EndpointId, String)>> = std::sync::Mutex::new(None);

pub fn set_session(id: iroh::EndpointId, name: &str) {
    *SESSION.lock().unwrap() = Some((id, name.to_string()));
}

pub fn open_for_session() {
    if let Some((id, name)) = SESSION.lock().unwrap().clone()
        && let Err(e) = open_window(id, &name)
    {
        crate::log!("terminal window: {e:#}");
    }
}

/// A terminal window with `remote-control shell` to `id`: Windows Terminal, else a console window.
pub fn open_window(id: iroh::EndpointId, _name: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x10;
    let exe = std::env::current_exe()?;
    let id = id.to_string();
    // Pass a fixed title to wt.exe because wt parses `;` in `--title` as a subcommand separator;
    // `shell_inner` sets the sanitized `{name} · 终端` title via OSC 0 once connected.
    // A GUI-subsystem child does not receive ConPTY automatically; `cmd /c start /wait /b`
    // hosts the ConPTY and waits while `remote-control shell` attaches to it.
    let wt = std::process::Command::new("wt.exe")
        .args(["-w", "new", "new-tab", "--title", "终端", "cmd.exe", "/d", "/c", "start", "remote control", "/wait", "/b"])
        .arg(&exe)
        .args(["shell", &id])
        .spawn();
    if wt.is_ok() {
        return Ok(());
    }
    std::process::Command::new(&exe).args(["shell", &id]).creation_flags(CREATE_NEW_CONSOLE).spawn()?;
    Ok(())
}

fn clean_title(s: &str) -> String {
    s.chars().filter(|&c| !c.is_control() && !('\u{007f}'..='\u{009f}').contains(&c)).collect()
}

/// Ensures an interactive `shell` invocation has a console when launched from a
/// GUI-subsystem parent (e.g. `CREATE_NEW_CONSOLE` without a parent console).
/// Preserves redirected/piped stdio so non-interactive callers still get the
/// `exec` guidance error instead of a stray console window.
fn ensure_console() {
    use windows::Win32::{
        Storage::FileSystem::{FILE_TYPE_UNKNOWN, GetFileType},
        System::Console::{
            ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole, GetConsoleWindow, GetStdHandle,
            STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        },
    };
    unsafe {
        if !GetConsoleWindow().0.is_null() {
            return;
        }
        let has_stdio = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE].into_iter().any(|kind| {
            GetStdHandle(kind)
                .ok()
                .is_some_and(|h| !h.is_invalid() && !h.0.is_null() && GetFileType(h) != FILE_TYPE_UNKNOWN)
        });
        if has_stdio {
            return;
        }
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            let _ = AllocConsole();
        }
    }
}

/// `remote-control shell`: PowerShell there, in this console window. A
/// failure stays on screen until a key, since the window closes after.
pub fn shell(device: &str) -> Result<i32> {
    ensure_console();
    let r = shell_inner(device);
    if let Err(e) = &r {
        eprintln!("\r\n{e:#}\r\n按回车关闭");
        let _ = std::io::stdin().read_line(&mut String::new());
        std::process::exit(255);
    }
    r
}

fn shell_inner(device: &str) -> Result<i32> {
    let rt = runtime()?;
    rt.block_on(async {
        let (id, name) = find(device)?;
        let safe_name = clean_title(&name);
        let raw = RawConsole::new()?;
        let (cols, rows) = raw.size();
        print!("\x1b]0;{safe_name} · 终端\x07");
        eprintln!("正在连接「{safe_name}」……\r");
        let open = Open { proof: String::new(), what: What::Shell, cols, rows, text: String::new(), cwd: String::new() };
        let Session { ep, conn, mut send, mut recv } = connect(id, open).await?;
        let (tx, mut keys) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        read_keys(raw.input, tx);
        let mut size = (cols, rows);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
        let mut carry = Vec::new();
        let code = loop {
            tokio::select! {
                k = keys.recv() => match k {
                    Some(k) => write_frame(&mut send, INPUT, &k).await?,
                    None => break 0,
                },
                _ = tick.tick() => {
                    let now = raw.size();
                    if now != size {
                        size = now;
                        write_frame(&mut send, RESIZE, &resize(now.0, now.1)).await?;
                    }
                }
                f = read_frame(&mut recv) => match f {
                    Ok(Some((OUTPUT, b))) => show(&mut carry, &b),
                    Ok(Some((EXIT, b))) => break Fields::new(&b).u32().unwrap_or(0) as i32,
                    Ok(Some(_)) => {}
                    _ => {
                        drop(raw);
                        bail!("和「{name}」的连接断开了");
                    }
                }
            }
        };
        drop(raw);
        bye(&conn, &ep).await;
        Ok(code)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_round_trips() {
        let o = Open { proof: "rcd1x".into(), what: What::Exec, cols: 80, rows: 25, text: "dir 你好".into(), cwd: r"D:\a".into() };
        assert_eq!(Open::decode(&o.encode()).unwrap(), o);
        assert_eq!(remote_side(r"laptop:D:\x"), Some(("laptop", r"D:\x")));
        assert_eq!(remote_side(r"C:\x"), None);
        assert_eq!(dest(Path::new("T"), true, "a/b").unwrap(), Path::new("T").join("a").join("b"));
        assert_eq!(dest(Path::new("T"), false, "a/b").unwrap(), Path::new("T").join("b"));
        assert!(dest(Path::new("T"), true, "a/../x").is_err());
    }

    #[test]
    fn skill_state_transitions() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let home = std::env::temp_dir().join(format!("rc-skill-test-{}-{nanos}", std::process::id()));
        let exe_a = Path::new(r"C:\a\remote-control.exe");
        let exe_b = Path::new(r"C:\b\remote-control.exe");

        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(skill_state_in(&home, exe_a), SkillState::NotInstalled);

        let where_msg = install_skill_in(&home, exe_a).unwrap();
        assert_eq!(where_msg, r"~\.claude\skills 和 ~\.agents\skills");
        assert_eq!(skill_state_in(&home, exe_a), SkillState::Installed);
        assert_eq!(skill_state_in(&home, exe_b), SkillState::Outdated);

        // Removing one target leaves it Outdated; uninstalling cleans up the rest.
        std::fs::remove_file(home.join(".claude").join("skills").join("remote-control").join("SKILL.md")).unwrap();
        assert_eq!(skill_state_in(&home, exe_a), SkillState::Outdated);

        uninstall_skill_in(&home).unwrap();
        assert_eq!(skill_state_in(&home, exe_a), SkillState::NotInstalled);
        uninstall_skill_in(&home).unwrap();

        let _ = std::fs::remove_dir_all(&home);
    }

    /// A real shell over the network: `RC_EXE` (this program), `RC_SHELL_ID` (a host that lets this computer in).
    #[test]
    #[ignore]
    fn shell_through_a_pty() {
        use std::io::Read;
        let (exe, id) = (std::env::var("RC_EXE").unwrap(), std::env::var("RC_SHELL_ID").unwrap());
        let (pty, io) = Pty::spawn_with(false, Some(&format!("\"{exe}\" shell {id}")), 100, 30).unwrap();
        let mut output = io.output;
        let reader = std::thread::spawn(move || {
            let mut all = Vec::new();
            let _ = output.read_to_end(&mut all);
            String::from_utf8_lossy(&all).into_owned()
        });
        let mut input = io.input;
        std::thread::sleep(std::time::Duration::from_secs(4));
        input.write_all("Write-Output ('x'+'yz'+'你好'); $Host.UI.RawUI.WindowSize.Width\r".as_bytes()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        input.write_all(b"exit 4\r").unwrap();
        let code = pty.wait();
        let text = reader.join().unwrap();
        println!("{text}");
        assert!(text.contains("xyz你好"));
        assert!(text.contains("100"));
        assert_eq!(code, 4);
    }
}
