//! Clipboard over the connection, the same on host and viewer. A local change
//! is announced on the control stream (`ClipOffer`, small) and its data goes
//! on a unidirectional stream of its own, below the control stream's
//! priority, so a large image does not hold up input.
//! Data stream: u8 kind (`proto::STREAM_CLIPBOARD`), u32 seq, u32 seen,
//! u8 format, u32 length, then the data.
//!
//! The newest copy wins (see `Order`): a transfer is dropped when a newer
//! offer came from the other side, or when the user copied something here
//! after it was offered. Each copy carries the newest seq the sender had
//! heard of from the other side (`seen`), so two copies made at the same
//! moment on both sides are told apart from one made after the other; the
//! host's copy wins such a tie, so both sides end up the same.
//!
//! Copied files go as their list (`clipfiles`); the side that pastes asks for
//! each file on a bidirectional stream, answered from that side's last few
//! copies only.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::Instant,
};

use anyhow::{Result, bail};
use bytes::Bytes;
use iroh::endpoint::{Connection, RecvStream};
use tokio::{sync::mpsc, task::AbortHandle};

use crate::{
    clipboard::{Clipboard, Content, FORMAT_FILES, MAX_TRANSFER, Remote},
    clipfiles::{self, Source},
    log,
    proto::{Msg, STREAM_AUDIO, STREAM_CLIPBOARD, STREAM_FILE},
};

/// Copies of files here whose files the other side may still read (a paste
/// in progress can outlast a newer copy).
const SERVED: usize = 8;

/// Which copy is the newest. Seqs count each side's own copies from 1.
#[derive(Debug)]
pub struct Order {
    host: bool,
    /// Our newest copy, and the newest remote seq we knew of when it was made.
    local: u32,
    local_seen: u32,
    /// The newest remote seq heard of (offer or data).
    remote: u32,
    applied: u32,
}

impl Order {
    pub fn new(host: bool) -> Order {
        Order { host, local: 0, local_seen: 0, remote: 0, applied: 0 }
    }

    /// The user copied here: the new copy's (seq, seen).
    pub fn local_copy(&mut self) -> (u32, u32) {
        self.local += 1;
        self.local_seen = self.remote;
        (self.local, self.local_seen)
    }

    pub fn heard(&mut self, seq: u32) {
        self.remote = self.remote.max(seq);
    }

    /// Whether remote copy `seq` can still be the newest (else stop receiving it).
    pub fn wanted(&self, seq: u32) -> bool {
        seq >= self.remote && seq > self.applied && self.local_seen < seq
    }

    /// Remote copy `seq`, made knowing our copy `seen`, has arrived: true to put it on the clipboard.
    pub fn apply(&mut self, seq: u32, seen: u32) -> bool {
        self.heard(seq);
        // Made after our newest copy reached the other side, or at the same
        // moment as it (neither knew of the other): then the host's wins.
        let ok = self.wanted(seq) && (seen >= self.local || !self.host);
        if ok {
            self.applied = seq;
        }
        ok
    }
}

pub struct ClipSync {
    clip: Clipboard,
    order: Mutex<Order>,
    /// Files copied here, by copy seq: where each entry of the list sent is.
    served: Mutex<VecDeque<(u32, Arc<Vec<PathBuf>>)>>,
    /// Files are read as the console user (a host running as the service).
    service: bool,
}

impl ClipSync {
    /// Starts watching the local clipboard and serving pasted files without
    /// claiming `conn.accept_uni()`. Pair with [`accept_uni_loop`] so audio
    /// uni-streams still run even if the clipboard fails to start.
    pub fn start_clip(
        conn: &Connection,
        ctrl: mpsc::UnboundedSender<Msg>,
        host: bool,
        service: bool,
    ) -> Result<Arc<ClipSync>> {
        let (changed_tx, changed_rx) = mpsc::unbounded_channel();
        let sync = Arc::new(ClipSync {
            clip: Clipboard::start(changed_tx, service)?,
            order: Mutex::new(Order::new(host)),
            served: Mutex::default(),
            service,
        });
        tokio::spawn(send_loop(conn.clone(), ctrl, changed_rx, Arc::downgrade(&sync)));
        tokio::spawn(serve_loop(conn.clone(), Arc::downgrade(&sync)));
        Ok(sync)
    }

    fn served_path(&self, seq: u32, index: u32) -> Option<PathBuf> {
        let s = self.served.lock().unwrap();
        s.iter().find(|(q, _)| *q == seq).and_then(|(_, p)| p.get(index as usize).cloned())
    }

    pub fn on_offer(&self, seq: u32) {
        self.order.lock().unwrap().heard(seq);
    }
}

struct AbortOnDrop(Option<AbortHandle>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

async fn send_loop(
    conn: Connection,
    ctrl: mpsc::UnboundedSender<Msg>,
    mut changed: mpsc::UnboundedReceiver<Content>,
    sync: Weak<ClipSync>,
) {
    let mut current = AbortOnDrop(None);
    loop {
        let content = tokio::select! {
            _ = conn.closed() => break,
            c = changed.recv() => match c {
                Some(c) => c,
                None => break,
            },
        };
        let mut what = content.describe();
        let (seq, seen, service) = {
            let Some(s) = sync.upgrade() else { break };
            let (seq, seen) = s.order.lock().unwrap().local_copy();
            (seq, seen, s.service)
        };
        let t0 = Instant::now();
        let encoded = match content {
            Content::Files(roots) => {
                let listed = tokio::task::spawn_blocking(move || crate::term_proc::as_user(service, || clipfiles::scan(&roots))?).await;
                match listed {
                    Ok(Ok((entries, paths))) => {
                        what = clipfiles::describe(&entries);
                        let Some(s) = sync.upgrade() else { break };
                        let mut served = s.served.lock().unwrap();
                        served.push_back((seq, Arc::new(paths)));
                        if served.len() > SERVED {
                            served.pop_front();
                        }
                        Ok(Ok((FORMAT_FILES, clipfiles::encode_list(&entries))))
                    }
                    Ok(Err(e)) => Ok(Err(e)),
                    Err(e) => Err(e),
                }
            }
            content => tokio::task::spawn_blocking(move || content.encode()).await,
        };
        let (format, data) = match encoded {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => {
                log!("clipboard: {what} not sent: {e:#}");
                continue;
            }
            Err(_) => continue,
        };
        if data.len() > MAX_TRANSFER {
            log!("clipboard: {what} not sent: {} MB is over the {} MB limit", data.len() >> 20, MAX_TRANSFER >> 20);
            continue;
        }
        if ctrl.send(Msg::ClipOffer { seq, seen, format, bytes: data.len() as u32 }).is_err() {
            break;
        }
        // A newer copy replaces one still being sent.
        if let Some(h) = current.0.take() {
            h.abort();
        }
        let conn = conn.clone();
        let encode_ms = t0.elapsed().as_millis();
        current.0 = Some(
            tokio::spawn(async move {
                let t1 = Instant::now();
                match send_one(&conn, seq, seen, format, &data).await {
                    Ok(()) => log!(
                        "clipboard: sent {what} as {} KB (encode {encode_ms} ms, send {} ms)",
                        data.len().div_ceil(1024),
                        t1.elapsed().as_millis()
                    ),
                    Err(e) => log!("clipboard: sending {what} failed: {e:#}"),
                }
            })
            .abort_handle(),
        );
    }
}

async fn send_one(conn: &Connection, seq: u32, seen: u32, format: u8, data: &[u8]) -> Result<()> {
    let mut s = conn.open_uni().await?;
    s.set_priority(-1)?;
    let mut head = vec![STREAM_CLIPBOARD];
    head.extend_from_slice(&seq.to_be_bytes());
    head.extend_from_slice(&seen.to_be_bytes());
    head.push(format);
    head.extend_from_slice(&(data.len() as u32).to_be_bytes());
    s.write_all(&head).await?;
    s.write_all(data).await?;
    s.finish()?;
    s.stopped().await?;
    Ok(())
}

/// Accepts incoming unidirectional streams on `conn`, dispatching clipboard
/// transfers to `clip` (when alive) and lossless audio frames to `audio_tx`.
pub async fn accept_uni_loop(
    conn: Connection,
    clip: Option<Weak<ClipSync>>,
    audio_tx: Option<mpsc::UnboundedSender<Bytes>>,
) {
    loop {
        let mut recv = tokio::select! {
            _ = conn.closed() => break,
            r = conn.accept_uni() => match r {
                Ok(r) => r,
                Err(_) => break,
            },
        };
        let (clip, conn, audio_tx) = (clip.clone(), conn.clone(), audio_tx.clone());
        tokio::spawn(async move {
            let mut kind = [0u8; 1];
            if recv.read_exact(&mut kind).await.is_err() {
                return;
            }
            match kind[0] {
                STREAM_CLIPBOARD => {
                    let Some(sync) = clip.as_ref().and_then(Weak::upgrade) else {
                        let _ = recv.stop(0u32.into());
                        return;
                    };
                    if let Err(e) = receive_clipboard(recv, &sync, &conn).await {
                        log!("clipboard: receive failed: {e:#}");
                    }
                }
                STREAM_AUDIO => {
                    if let Some(tx) = audio_tx {
                        crate::audio::wasapi::recv_lossless_stream(recv, tx).await;
                    } else {
                        let _ = recv.stop(0u32.into());
                    }
                }
                other => {
                    let _ = recv.stop(0u32.into());
                    log!("uni stream: unknown kind {other}");
                }
            }
        });
    }
}

/// The other side pasting files copied here asks for them one by one.
async fn serve_loop(conn: Connection, sync: Weak<ClipSync>) {
    loop {
        let (send, mut recv) = tokio::select! {
            _ = conn.closed() => break,
            r = conn.accept_bi() => match r {
                Ok(s) => s,
                Err(_) => break,
            },
        };
        let sync = sync.clone();
        tokio::spawn(async move {
            let mut kind = [0u8; 1];
            if recv.read_exact(&mut kind).await.is_err() || kind[0] != STREAM_FILE {
                let _ = recv.stop(0u32.into());
                return;
            }
            let Some(sync) = sync.upgrade() else {
                let _ = recv.stop(0u32.into());
                return;
            };
            let t0 = Instant::now();
            let lookup = |seq, index| sync.served_path(seq, index);
            match clipfiles::serve(send, recv, lookup, sync.service).await {
                Ok(n) => log!("clipboard: sent a pasted file ({} KB in {} ms)", n.div_ceil(1024), t0.elapsed().as_millis()),
                Err(e) => log!("clipboard: sending a pasted file failed: {e:#}"),
            }
        });
    }
}

async fn receive_clipboard(mut r: RecvStream, sync: &ClipSync, conn: &Connection) -> Result<()> {
    let mut head = [0u8; 13];
    r.read_exact(&mut head).await?;
    let be = |i: usize| u32::from_be_bytes(head[i..i + 4].try_into().expect("4 bytes"));
    let (seq, seen, format, len) = (be(0), be(4), head[8], be(9) as usize);
    if len > MAX_TRANSFER {
        let _ = r.stop(0u32.into());
        bail!("{len} bytes is over the limit");
    }
    sync.order.lock().unwrap().heard(seq);
    let superseded = || !sync.order.lock().unwrap().wanted(seq);
    let t0 = Instant::now();
    let mut data = Vec::with_capacity(len);
    while data.len() < len {
        if superseded() {
            let _ = r.stop(0u32.into());
            log!("clipboard: dropped transfer {seq}, a newer copy exists");
            return Ok(());
        }
        match r.read_chunk(len - data.len()).await? {
            Some(c) => data.extend_from_slice(&c),
            None => bail!("transfer {seq} ended after {} of {len} bytes", data.len()),
        }
    }
    let recv_ms = t0.elapsed().as_millis();
    let content = if format == FORMAT_FILES {
        let entries = Arc::new(clipfiles::decode_list(&data)?);
        let src = Arc::new(Source { conn: conn.clone(), rt: tokio::runtime::Handle::current(), seq });
        Content::Remote(Remote { src, entries })
    } else {
        tokio::task::spawn_blocking(move || Content::decode(format, &data)).await??
    };
    if !sync.order.lock().unwrap().apply(seq, seen) {
        log!("clipboard: dropped {}, a newer copy exists", content.describe());
        return Ok(());
    }
    log!("clipboard: received {} ({} KB in {recv_ms} ms)", content.describe(), len.div_ceil(1024));
    sync.clip.set(content);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_made_after_the_other_arrived_wins() {
        let (mut h, mut v) = (Order::new(true), Order::new(false));
        let (s, seen) = v.local_copy();
        assert!(h.apply(s, seen));
        // The host copies after that: the viewer takes it.
        let (s, seen) = h.local_copy();
        assert_eq!(seen, 1);
        v.heard(s);
        assert!(v.apply(s, seen));
        // And the viewer may copy again on top.
        let (s, seen) = v.local_copy();
        assert!(h.apply(s, seen));
    }

    #[test]
    fn a_local_copy_while_a_transfer_is_on_its_way_wins() {
        let (mut h, mut v) = (Order::new(true), Order::new(false));
        let (s, seen) = h.local_copy();
        v.heard(s);
        // The user copies on the viewer before the host's image has arrived.
        let (vs, vseen) = v.local_copy();
        assert_eq!(vseen, 1);
        assert!(!v.wanted(s));
        assert!(!v.apply(s, seen));
        // The host takes the viewer's copy: it was made knowing the host's.
        assert!(h.apply(vs, vseen));
    }

    #[test]
    fn copies_at_the_same_moment_end_the_same_on_both_sides() {
        let (mut h, mut v) = (Order::new(true), Order::new(false));
        let (hs, hseen) = h.local_copy();
        let (vs, vseen) = v.local_copy();
        // Neither knew of the other: the host keeps its own, the viewer takes it.
        assert!(!h.apply(vs, vseen));
        assert!(v.apply(hs, hseen));
    }

    #[test]
    fn only_the_newest_remote_copy_is_applied() {
        let mut v = Order::new(false);
        v.heard(1);
        v.heard(2);
        assert!(!v.wanted(1));
        assert!(!v.apply(1, 0));
        assert!(v.apply(2, 0));
        assert!(!v.apply(2, 0));
    }
}
