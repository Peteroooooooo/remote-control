//! Files and folders on the clipboard, copied on one side and pasted on the
//! other. A copy sends only the list (names, sizes, times); a paste reads each
//! file from the other side while Explorer copies it, so nothing moves until
//! something is pasted and Explorer shows its own progress, cancel and
//! name-clash questions.
//!
//! The pasting side puts a data object on its clipboard with "virtual files"
//! (FileGroupDescriptorW, FileContents as an IStream per file). Each stream
//! opens a bidirectional stream on the session connection at its first read:
//! `u8 STREAM_FILE, u32 seq, u32 index, u64 offset`; the answer is `u8 0, u64
//! length` then the bytes, or `u8 1, u32 length` and a message. Only files of
//! the other side's own recent copies are served (`seq`, `index` into its list).

use std::{
    ffi::OsString,
    io::{Read as _, Seek, SeekFrom},
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use windows::{
    Win32::{
        Foundation::{
            DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_FAIL, E_INVALIDARG, E_NOTIMPL, FILETIME, GlobalFree,
            HGLOBAL, OLE_E_ADVISENOTSUPPORTED, S_OK, STG_E_ACCESSDENIED,
        },
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
        System::{
            Com::*,
            DataExchange::RegisterClipboardFormatW,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::{DROPEFFECT_COPY, ReleaseStgMedium},
        },
        UI::Shell::{
            CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_PREFERREDDROPEFFECT, FD_ATTRIBUTES, FD_FILESIZE,
            FD_PROGRESSUI, FD_WRITESTIME, FILEDESCRIPTORW, SHCreateStdEnumFmtEtc,
        },
    },
    core::{HRESULT, PCWSTR},
};
use windows_core::{BOOL, Ref, implement};

use crate::{log, proto::STREAM_FILE};

/// Most entries (files and folders) in one copy; Explorer's list of them is 592 bytes each.
pub const MAX_ENTRIES: usize = 50_000;
/// Explorer's names of virtual files are at most 259 UTF-16 units, folders included.
const MAX_NAME: usize = 259;
/// A stalled transfer fails instead of hanging Explorer's copy.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const CHUNK: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Entry {
    /// Path from the copied item's parent, `/` between the parts.
    pub rel: String,
    pub dir: bool,
    pub size: u64,
    /// Last write time, as a FILETIME (100 ns since 1601).
    pub mtime: u64,
}

pub fn describe(entries: &[Entry]) -> String {
    let files = entries.iter().filter(|e| !e.dir).count();
    let bytes: u64 = entries.iter().map(|e| e.size).sum();
    format!("{files} files, {} folders ({:.1} MB)", entries.len() - files, bytes as f64 / 1e6)
}

fn filetime(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| (d.as_nanos() / 100) as u64 + 116_444_736_000_000_000)
}

fn name_ok(rel: &str) -> bool {
    rel.encode_utf16().count() <= MAX_NAME
        && rel.split('/').all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains([':', '\\', '\0']))
}

/// The copied files and folders, with everything in the folders: the list
/// sent, and where each entry is here. Blocking (the host's runs as its user).
pub fn scan(roots: &[PathBuf]) -> Result<(Vec<Entry>, Vec<PathBuf>)> {
    let (mut entries, mut paths) = (Vec::new(), Vec::new());
    fn walk(path: &Path, rel: String, entries: &mut Vec<Entry>, paths: &mut Vec<PathBuf>) -> Result<()> {
        if entries.len() >= MAX_ENTRIES {
            bail!("more than {MAX_ENTRIES} files and folders");
        }
        if !name_ok(&rel) {
            bail!("{} has a path longer than {MAX_NAME} characters", path.display());
        }
        let meta = std::fs::metadata(path).with_context(|| path.display().to_string())?;
        let mtime = meta.modified().map_or(0, filetime);
        let dir = meta.is_dir();
        entries.push(Entry { rel: rel.clone(), dir, size: if dir { 0 } else { meta.len() }, mtime });
        paths.push(path.to_path_buf());
        if dir {
            let mut list: Vec<_> = std::fs::read_dir(path)
                .with_context(|| path.display().to_string())?
                .filter_map(|e| e.ok())
                .collect();
            list.sort_by_key(|e| e.file_name());
            for e in list {
                walk(&e.path(), format!("{rel}/{}", e.file_name().to_string_lossy()), entries, paths)?;
            }
        }
        Ok(())
    }
    for r in roots {
        let name = r.file_name().ok_or_else(|| anyhow!("{} is a whole drive", r.display()))?;
        walk(r, name.to_string_lossy().into_owned(), &mut entries, &mut paths)?;
    }
    Ok((entries, paths))
}

pub fn encode_list(entries: &[Entry]) -> Vec<u8> {
    let mut v = (entries.len() as u32).to_be_bytes().to_vec();
    for e in entries {
        v.push(e.dir as u8);
        v.extend_from_slice(&e.size.to_be_bytes());
        v.extend_from_slice(&e.mtime.to_be_bytes());
        v.extend_from_slice(&(e.rel.len() as u32).to_be_bytes());
        v.extend_from_slice(e.rel.as_bytes());
    }
    v
}

pub fn decode_list(b: &[u8]) -> Result<Vec<Entry>> {
    let mut at = 0usize;
    let mut take = |n: usize| -> Result<&[u8]> {
        let s = b.get(at..at + n).context("file list too short")?;
        at += n;
        Ok(s)
    };
    let n = u32::from_be_bytes(take(4)?.try_into()?) as usize;
    if n == 0 || n > MAX_ENTRIES {
        bail!("file list of {n} entries");
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let dir = take(1)?[0] != 0;
        let size = u64::from_be_bytes(take(8)?.try_into()?);
        let mtime = u64::from_be_bytes(take(8)?.try_into()?);
        let len = u32::from_be_bytes(take(4)?.try_into()?) as usize;
        let rel = String::from_utf8(take(len)?.to_vec())?;
        if !name_ok(&rel) {
            bail!("bad path {rel:?} in the file list");
        }
        out.push(Entry { rel, dir, size, mtime });
    }
    Ok(out)
}

/// The paths in CF_HDROP data (what Explorer puts on the clipboard): a
/// DROPFILES header (offset of the list, point, two flags), then the paths,
/// each ending with a NUL, and one more NUL.
pub fn dropfiles_paths(b: &[u8]) -> Vec<PathBuf> {
    let at = |i: usize| b.get(i..i + 4).map(|s| u32::from_le_bytes(s.try_into().expect("4 bytes")));
    let (Some(start), Some(wide)) = (at(0), at(16)) else { return Vec::new() };
    let Some(list) = b.get(start as usize..) else { return Vec::new() };
    let text: Vec<String> = if wide != 0 {
        let units: Vec<u16> = list.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        units.split(|&u| u == 0).take_while(|p| !p.is_empty()).map(|p| OsString::from_wide(p).to_string_lossy().into_owned()).collect()
    } else {
        list.split(|&c| c == 0).take_while(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect()
    };
    text.into_iter().map(PathBuf::from).collect()
}

// ---- Serving: the side that copied ----

/// Answers one file request whose kind byte was read; `path` finds the file.
/// The number of bytes sent.
pub async fn serve(mut send: SendStream, mut recv: RecvStream, path: impl FnOnce(u32, u32) -> Option<PathBuf>, service: bool) -> Result<u64> {
    let mut head = [0u8; 16];
    recv.read_exact(&mut head).await?;
    let seq = u32::from_be_bytes(head[0..4].try_into()?);
    let index = u32::from_be_bytes(head[4..8].try_into()?);
    let offset = u64::from_be_bytes(head[8..16].try_into()?);
    let opened = match path(seq, index) {
        None => Err(anyhow!("这份复制的文件已经不在了，请重新复制")),
        Some(p) => tokio::task::spawn_blocking(move || -> Result<(std::fs::File, u64)> {
            let mut f = crate::term_proc::as_user(service, || std::fs::File::open(&p))?
                .map_err(|e| anyhow!("{}：{e}", p.display()))?;
            let len = f.metadata()?.len();
            f.seek(SeekFrom::Start(offset.min(len)))?;
            Ok((f, len.saturating_sub(offset)))
        })
        .await?,
    };
    let (mut file, mut left) = match opened {
        Ok(o) => o,
        Err(e) => {
            let msg = format!("{e:#}");
            let mut v = vec![1u8];
            v.extend_from_slice(&(msg.len() as u32).to_be_bytes());
            v.extend_from_slice(msg.as_bytes());
            send.write_all(&v).await?;
            send.finish()?;
            return Err(e);
        }
    };
    // Below input and the clipboard's small copies.
    send.set_priority(-2)?;
    let mut v = vec![0u8];
    v.extend_from_slice(&left.to_be_bytes());
    send.write_all(&v).await?;
    let total = left;
    while left > 0 {
        let want = CHUNK.min(left as usize);
        let (f, buf) = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; want];
            let n = file.read(&mut buf);
            (file, n.map(|n| {
                buf.truncate(n);
                buf
            }))
        })
        .await?;
        file = f;
        let buf = buf?;
        if buf.is_empty() {
            let _ = send.reset(1u32.into());
            bail!("file {index} of copy {seq} got shorter while being sent");
        }
        send.write_all(&buf).await?;
        left -= buf.len() as u64;
    }
    send.finish()?;
    Ok(total)
}

// ---- Pasting: the other side's files on this clipboard ----

/// Where the files of one copy on the other side come from.
pub struct Source {
    pub conn: Connection,
    pub rt: tokio::runtime::Handle,
    pub seq: u32,
}

async fn open(conn: &Connection, seq: u32, index: u32, offset: u64) -> Result<(RecvStream, u64)> {
    let (mut send, mut recv) = conn.open_bi().await?;
    let mut req = vec![STREAM_FILE];
    req.extend_from_slice(&seq.to_be_bytes());
    req.extend_from_slice(&index.to_be_bytes());
    req.extend_from_slice(&offset.to_be_bytes());
    send.write_all(&req).await?;
    send.finish()?;
    let mut status = [0u8; 1];
    recv.read_exact(&mut status).await?;
    if status[0] == 0 {
        let mut len = [0u8; 8];
        recv.read_exact(&mut len).await?;
        return Ok((recv, u64::from_be_bytes(len)));
    }
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let mut msg = vec![0u8; (u32::from_be_bytes(len) as usize).min(4096)];
    recv.read_exact(&mut msg).await?;
    bail!("{}", String::from_utf8_lossy(&msg))
}

struct StreamState {
    pos: u64,
    /// The open transfer and how much of it is still to come.
    open: Option<(RecvStream, u64)>,
}

#[implement(IStream)]
struct FileStream {
    src: Arc<Source>,
    index: u32,
    entry: Entry,
    st: Mutex<StreamState>,
}

impl FileStream {
    /// Fills `buf` from the current position; fewer bytes only at the end.
    fn read_into(&self, buf: &mut [u8]) -> Result<usize> {
        let mut st = self.st.lock().unwrap();
        if st.pos >= self.entry.size || buf.is_empty() {
            return Ok(0);
        }
        let src = &self.src;
        let pos = st.pos;
        let n = src.rt.block_on(async {
            if st.open.is_none() {
                st.open = Some(tokio::time::timeout(READ_TIMEOUT, open(&src.conn, src.seq, self.index, pos)).await??);
            }
            let (recv, left) = st.open.as_mut().expect("opened");
            let want = buf.len().min(*left as usize);
            let mut got = 0;
            while got < want {
                match tokio::time::timeout(READ_TIMEOUT, recv.read(&mut buf[got..want])).await? {
                    Ok(Some(k)) => got += k,
                    Ok(None) => break,
                    Err(e) => return Err(anyhow!(e)),
                }
            }
            *left -= got as u64;
            if got < want {
                bail!("the transfer ended {} bytes early", *left);
            }
            anyhow::Ok(got)
        });
        match n {
            Ok(n) => {
                st.pos += n as u64;
                Ok(n)
            }
            Err(e) => {
                st.open = None;
                Err(e)
            }
        }
    }
}

impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return E_INVALIDARG;
        }
        let buf = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
        let r = self.read_into(buf);
        let n = *r.as_ref().unwrap_or(&0);
        if !pcbread.is_null() {
            unsafe { *pcbread = n as u32 };
        }
        match r {
            Ok(_) => S_OK,
            Err(e) => {
                log!("clipboard: pasting {} failed: {e:#}", self.entry.rel);
                E_FAIL
            }
        }
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> windows_core::Result<()> {
        let mut st = self.st.lock().unwrap();
        let base = match dworigin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => st.pos as i64,
            STREAM_SEEK_END => self.entry.size as i64,
            _ => return Err(E_INVALIDARG.into()),
        };
        let to = base.checked_add(dlibmove).filter(|&p| p >= 0).ok_or(windows_core::Error::from(E_INVALIDARG))? as u64;
        if to != st.pos {
            st.open = None;
            st.pos = to;
        }
        if !plibnewposition.is_null() {
            unsafe { *plibnewposition = to };
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows_core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(&self, pstm: Ref<IStream>, cb: u64, pcbread: *mut u64, pcbwritten: *mut u64) -> windows_core::Result<()> {
        let to = pstm.ok()?;
        let (mut read, mut written) = (0u64, 0u64);
        let mut buf = vec![0u8; CHUNK];
        while read < cb {
            let want = CHUNK.min((cb - read) as usize);
            let n = self.read_into(&mut buf[..want]).map_err(|e| {
                log!("clipboard: pasting {} failed: {e:#}", self.entry.rel);
                windows_core::Error::from(E_FAIL)
            })?;
            if n == 0 {
                break;
            }
            read += n as u64;
            let mut w = 0u32;
            unsafe { to.Write(buf.as_ptr() as _, n as u32, Some(&mut w)) }.ok()?;
            written += w as u64;
        }
        unsafe {
            if !pcbread.is_null() {
                *pcbread = read;
            }
            if !pcbwritten.is_null() {
                *pcbwritten = written;
            }
        }
        Ok(())
    }

    fn Commit(&self, _: &STGC) -> windows_core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows_core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> windows_core::Result<()> {
        if pstatstg.is_null() {
            return Err(E_INVALIDARG.into());
        }
        let mut name = windows_core::PWSTR::null();
        if *grfstatflag != STATFLAG_NONAME {
            let leaf = self.entry.rel.rsplit('/').next().unwrap_or_default();
            let w: Vec<u16> = leaf.encode_utf16().chain([0]).collect();
            unsafe {
                let p = CoTaskMemAlloc(w.len() * 2) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                    name = windows_core::PWSTR(p);
                }
            }
        }
        let t = ft(self.entry.mtime);
        unsafe {
            *pstatstg = STATSTG {
                pwcsName: name,
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.entry.size,
                mtime: t,
                ctime: t,
                atime: t,
                grfMode: STGM_READ,
                ..Default::default()
            };
        }
        Ok(())
    }

    fn Clone(&self) -> windows_core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

fn ft(v: u64) -> FILETIME {
    FILETIME { dwLowDateTime: v as u32, dwHighDateTime: (v >> 32) as u32 }
}

fn global(bytes: &[u8]) -> windows_core::Result<HGLOBAL> {
    unsafe {
        let g = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let p = GlobalLock(g) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(g));
            return Err(E_FAIL.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(g);
        Ok(g)
    }
}

fn format(name: PCWSTR) -> u16 {
    unsafe { RegisterClipboardFormatW(name) as u16 }
}

fn formatetc(cf: u16, lindex: i32, tymed: TYMED) -> FORMATETC {
    FORMATETC { cfFormat: cf, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex, tymed: tymed.0 as u32 }
}

/// The other side's copied files, as this clipboard's data object.
#[implement(IDataObject)]
pub struct FilesData {
    src: Arc<Source>,
    entries: Arc<Vec<Entry>>,
    cf_desc: u16,
    cf_contents: u16,
    cf_effect: u16,
}

pub fn data_object(src: Arc<Source>, entries: Arc<Vec<Entry>>) -> IDataObject {
    FilesData {
        src,
        entries,
        cf_desc: format(CFSTR_FILEDESCRIPTORW),
        cf_contents: format(CFSTR_FILECONTENTS),
        cf_effect: format(CFSTR_PREFERREDDROPEFFECT),
    }
    .into()
}

impl FilesData {
    fn descriptors(&self) -> Vec<u8> {
        let size = size_of::<FILEDESCRIPTORW>();
        let mut out = Vec::with_capacity(4 + self.entries.len() * size);
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in self.entries.iter() {
            let mut d = FILEDESCRIPTORW {
                dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_WRITESTIME.0 | FD_PROGRESSUI.0) as u32,
                dwFileAttributes: if e.dir { FILE_ATTRIBUTE_DIRECTORY.0 } else { FILE_ATTRIBUTE_NORMAL.0 },
                ftLastWriteTime: ft(e.mtime),
                nFileSizeHigh: (e.size >> 32) as u32,
                nFileSizeLow: e.size as u32,
                ..Default::default()
            };
            for (i, u) in e.rel.replace('/', "\\").encode_utf16().take(MAX_NAME).enumerate() {
                d.cFileName[i] = u;
            }
            let raw = unsafe { std::slice::from_raw_parts(&d as *const FILEDESCRIPTORW as *const u8, size) };
            out.extend_from_slice(raw);
        }
        out
    }

    fn check(&self, f: &FORMATETC) -> Result<(), HRESULT> {
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_DVASPECT);
        }
        let (tymed, ok_index) = if f.cfFormat == self.cf_contents {
            (TYMED_ISTREAM, usize::try_from(f.lindex).is_ok_and(|i| self.entries.get(i).is_some_and(|e| !e.dir)))
        } else if f.cfFormat == self.cf_desc || f.cfFormat == self.cf_effect {
            (TYMED_HGLOBAL, true)
        } else {
            return Err(DV_E_FORMATETC);
        };
        if f.tymed & tymed.0 as u32 == 0 {
            return Err(DV_E_TYMED);
        }
        if !ok_index {
            return Err(DV_E_LINDEX);
        }
        Ok(())
    }
}

impl IDataObject_Impl for FilesData_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows_core::Result<STGMEDIUM> {
        let f = unsafe { pformatetcin.as_ref() }.ok_or(windows_core::Error::from(E_INVALIDARG))?;
        self.check(f)?;
        if f.cfFormat == self.cf_contents {
            let entry = self.entries[f.lindex as usize].clone();
            let stream: IStream = FileStream {
                src: self.src.clone(),
                index: f.lindex as u32,
                entry,
                st: Mutex::new(StreamState { pos: 0, open: None }),
            }
            .into();
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 { pstm: std::mem::ManuallyDrop::new(Some(stream)) },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            });
        }
        let bytes = if f.cfFormat == self.cf_desc { self.descriptors() } else { DROPEFFECT_COPY.0.to_le_bytes().to_vec() };
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: global(&bytes)? },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        match unsafe { pformatetc.as_ref() } {
            None => E_INVALIDARG,
            Some(f) => self.check(f).err().unwrap_or(S_OK),
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        if !pformatetcout.is_null() {
            unsafe { (*pformatetcout).ptd = std::ptr::null_mut() };
        }
        E_NOTIMPL
    }

    /// Explorer reports how the paste went ("Performed DropEffect" and the like); nothing to keep.
    fn SetData(&self, _: *const FORMATETC, pmedium: *const STGMEDIUM, frelease: BOOL) -> windows_core::Result<()> {
        if frelease.as_bool() && !pmedium.is_null() {
            unsafe { ReleaseStgMedium(pmedium as *mut STGMEDIUM) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows_core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let list = [
            formatetc(self.cf_desc, -1, TYMED_HGLOBAL),
            formatetc(self.cf_contents, -1, TYMED_ISTREAM),
            formatetc(self.cf_effect, -1, TYMED_HGLOBAL),
        ];
        unsafe { SHCreateStdEnumFmtEtc(&list) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> windows_core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows_core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows_core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_round_trip_and_refusals() {
        let list = vec![
            Entry { rel: "资料".into(), dir: true, size: 0, mtime: 1 },
            Entry { rel: "资料/a b.txt".into(), dir: false, size: 5 << 32, mtime: 2 },
        ];
        assert_eq!(decode_list(&encode_list(&list)).unwrap(), list);
        for bad in ["../x", "a//b", "C:/x", "a/./b", "a\\b", ""] {
            let l = vec![Entry { rel: bad.into(), dir: false, size: 0, mtime: 0 }];
            assert!(decode_list(&encode_list(&l)).is_err(), "{bad:?}");
        }
        let long = vec![Entry { rel: "x".repeat(MAX_NAME + 1), dir: false, size: 0, mtime: 0 }];
        assert!(decode_list(&encode_list(&long)).is_err());
        assert!(decode_list(&encode_list(&[])).is_err());
        assert!(decode_list(&encode_list(&list)[..20]).is_err());
    }

    #[test]
    fn scan_lists_folders_before_their_files() {
        let root = std::env::temp_dir().join(format!("rc-clipfiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("top/sub")).unwrap();
        std::fs::write(root.join("top/b.txt"), b"hello").unwrap();
        std::fs::write(root.join("top/sub/c.bin"), [0u8; 3]).unwrap();
        std::fs::write(root.join("one.txt"), b"1").unwrap();
        let (entries, paths) = scan(&[root.join("top"), root.join("one.txt")]).unwrap();
        let rels: Vec<_> = entries.iter().map(|e| (e.rel.as_str(), e.dir, e.size)).collect();
        assert_eq!(rels, vec![("top", true, 0), ("top/b.txt", false, 5), ("top/sub", true, 0), ("top/sub/c.bin", false, 3), ("one.txt", false, 1)]);
        assert_eq!(paths[3], root.join("top").join("sub").join("c.bin"));
        assert!(entries.iter().all(|e| e.mtime > 116_444_736_000_000_000));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn dropfiles_wide_and_narrow() {
        let mut b = vec![20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0];
        for p in [r"C:\资料\a.txt", r"D:\x"] {
            b.extend(p.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        }
        b.extend([0, 0]);
        assert_eq!(dropfiles_paths(&b), vec![PathBuf::from(r"C:\资料\a.txt"), PathBuf::from(r"D:\x")]);
        let mut narrow = b[..20].to_vec();
        narrow[16] = 0;
        narrow.extend(b"C:\\a\0\0");
        assert_eq!(dropfiles_paths(&narrow), vec![PathBuf::from(r"C:\a")]);
        assert!(dropfiles_paths(&b[..10]).is_empty());
    }
}
