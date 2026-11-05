//! File drags between two machines. No OS calls beyond `std::fs`: the platform layer reports when a
//! file drag crossed the edge (`DragPump::start`), when the primary button came up here
//! (`DragPump::on_primary_up`) and when the cursor carried a drag away (`DragPump::on_cross_away`).
//!
//! The sender streams every file as soon as the drag crosses, so the data is waiting by the time the
//! user lets go. The recipient stages it, and moves it into the destination folder only on a drop;
//! a drag that comes back, or a link that ends, deletes what was staged.
//!
//! Names from the peer are a trust boundary: `safe_name` turns each into one path component that is
//! valid on macOS and Windows, and staged data never leaves the staging folder under any other name.

use flowseam_core::proto::{FileMeta, Msg, MAX_CHUNK_BYTES, MAX_DRAG_FILES, MAX_FILE_NAME_BYTES};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Unacknowledged file bytes the sender may have in flight. Bounds how far file data can sit ahead of a
/// mouse movement in the shared link's queues: 256 KiB is about 40 ms on a 50 Mbit/s Wi-Fi link.
pub const WINDOW_BYTES: u64 = 256 * 1024;
/// A recipient that acknowledges nothing for this long is gone; the sender gives up.
pub const STALL: Duration = Duration::from_secs(15);
/// Space a received drag must leave free on the destination volume.
pub const RESERVE_BYTES: u64 = 1 << 30;

/// Free bytes on the volume holding a path, as the platform measures it.
pub type FreeSpace = Arc<dyn Fn(&Path) -> Result<u64, String> + Send + Sync>;

fn human(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n => format!("{n} bytes"),
    }
}

/// One path component that is safe to create on macOS and Windows, made from a name the peer sent.
pub fn safe_name(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // Windows drops trailing dots and spaces, so "a." would silently become "a" there.
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    if s.is_empty() {
        s = "file".into();
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if device {
        s.insert(0, '_');
    }
    fit(&s, "", MAX_FILE_NAME_BYTES)
}

/// `head` cut at a char boundary so that `head + tail` fits in `max` bytes.
fn fit(head: &str, tail: &str, max: usize) -> String {
    let room = max.saturating_sub(tail.len());
    let mut end = head.len().min(room);
    while !head.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{tail}", &head[..end])
}

/// Claim a name in `dir` that nothing uses: `name`, then `stem (1).ext`, `stem (2).ext`, ... The claim is
/// an empty file created with `create_new`, so two writers can never pick the same name.
pub fn claim_unique(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 0..10_000u32 {
        let cand = if n == 0 { name.to_string() } else { fit(stem, &format!(" ({n}){ext}"), MAX_FILE_NAME_BYTES) };
        let path = dir.join(&cand);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("無法在 {} 建立 {cand}：{e}", dir.display())),
        }
    }
    Err(format!("{} 裡同名檔案太多：{name}", dir.display()))
}

/// Move a finished staged file onto its claimed name. Staging and destination can sit on different
/// volumes, where rename fails; then the data is copied, and the log says so.
fn land(staged: &Path, dest_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let target = claim_unique(dest_dir, name)?;
    if let Err(e) = fs::rename(staged, &target) {
        eprintln!("flowseam files: rename to {} failed ({e}); copying instead", target.display());
        if let Err(e) = fs::copy(staged, &target) {
            let _ = fs::remove_file(&target);
            return Err(format!("無法存到 {}：{e}", target.display()));
        }
        let _ = fs::remove_file(staged);
    }
    Ok(target)
}

/// Sending side of one drag.
pub struct Outgoing {
    drag: u32,
    files: Vec<(PathBuf, u64)>,
    idx: usize,
    open: Option<File>,
    left_in_file: u64,
    sent: u64,
    acked: u64,
    total: u64,
}

impl Outgoing {
    /// The regular files among `paths`, with their sizes now, and the `DragBegin` announcing them.
    /// Directories and anything unreadable are left out and named in `skipped`.
    pub fn new(drag: u32, paths: &[PathBuf]) -> Result<(Outgoing, Msg, Vec<String>), String> {
        let mut files = Vec::new();
        let mut skipped = Vec::new();
        for p in paths {
            match fs::metadata(p) {
                Ok(m) if m.is_file() => files.push((p.clone(), m.len())),
                // ponytail: folders are skipped; send their files with relative paths when a folder drag is wanted.
                Ok(_) => skipped.push(format!("{}（資料夾還不支援）", p.display())),
                Err(e) => skipped.push(format!("{}（{e}）", p.display())),
            }
        }
        files.truncate(MAX_DRAG_FILES);
        if files.is_empty() {
            return Err("拖曳裡沒有可以傳的檔案".into());
        }
        let metas = files
            .iter()
            .map(|(p, size)| FileMeta {
                name: safe_name(&p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
                size: *size,
            })
            .collect();
        let total = files.iter().map(|f| f.1).sum();
        let mut o = Outgoing { drag, files, idx: 0, open: None, left_in_file: 0, sent: 0, acked: 0, total };
        o.skip_empty();
        Ok((o, Msg::DragBegin { drag, files: metas }, skipped))
    }

    fn skip_empty(&mut self) {
        while self.idx < self.files.len() && self.files[self.idx].1 == 0 {
            self.idx += 1;
        }
    }

    pub fn on_ack(&mut self, bytes: u64) {
        self.acked = self.acked.max(bytes.min(self.sent));
    }

    /// The next chunk if the window has room; `Ok(None)` while waiting for acks or once everything is sent.
    /// A file that is shorter than when the drag began is an error: what was announced cannot be sent.
    pub fn next_chunk(&mut self) -> Result<Option<Msg>, String> {
        if self.idx >= self.files.len() || self.sent - self.acked >= WINDOW_BYTES {
            return Ok(None);
        }
        let (path, size) = &self.files[self.idx];
        if self.open.is_none() {
            self.open = Some(File::open(path).map_err(|e| format!("讀不到 {}：{e}", path.display()))?);
            self.left_in_file = *size;
        }
        let want = self.left_in_file.min(MAX_CHUNK_BYTES as u64) as usize;
        let mut data = vec![0u8; want];
        let f = self.open.as_mut().expect("opened above");
        let mut got = 0;
        while got < want {
            match f.read(&mut data[got..]) {
                Ok(0) => return Err(format!("{} 在傳送中變短了", path.display())),
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("讀取 {} 失敗：{e}", path.display())),
            }
        }
        let file = self.idx as u16;
        self.left_in_file -= want as u64;
        self.sent += want as u64;
        if self.left_in_file == 0 {
            self.open = None;
            self.idx += 1;
            self.skip_empty();
        }
        Ok(Some(Msg::FileChunk { drag: self.drag, file, data }))
    }

    pub fn all_sent(&self) -> bool {
        self.idx >= self.files.len()
    }

    pub fn all_acked(&self) -> bool {
        self.all_sent() && self.acked == self.total
    }
}

/// Receiving side of one drag: files staged under their own folder, moved out on a drop.
pub struct Incoming {
    drag: u32,
    dir: PathBuf,
    dest: PathBuf,
    files: Vec<FileMeta>,
    /// File the next chunk must belong to.
    idx: usize,
    open: Option<File>,
    got_in_file: u64,
    got: u64,
    dropped: bool,
    landed: Vec<Option<PathBuf>>,
}

impl Incoming {
    pub fn begin(staging_root: &Path, dest: &Path, drag: u32, files: &[FileMeta]) -> Result<Incoming, String> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let dir = staging_root.join(format!("{drag}-{nanos}"));
        fs::create_dir_all(&dir).map_err(|e| format!("無法建立暫存資料夾 {}：{e}", dir.display()))?;
        let files: Vec<FileMeta> = files.iter().map(|f| FileMeta { name: safe_name(&f.name), size: f.size }).collect();
        let mut inc = Incoming { drag, dir, dest: dest.to_path_buf(), landed: vec![None; files.len()], files, idx: 0, open: None, got_in_file: 0, got: 0, dropped: false };
        for i in 0..inc.files.len() {
            if inc.files[i].size == 0 {
                File::create(inc.staged(i)).map_err(|e| format!("無法建立暫存檔：{e}"))?;
            }
        }
        inc.skip_empty();
        Ok(inc)
    }

    pub fn drag(&self) -> u32 {
        self.drag
    }

    fn staged(&self, i: usize) -> PathBuf {
        self.dir.join(format!("{i}.part"))
    }

    fn skip_empty(&mut self) {
        while self.idx < self.files.len() && self.files[self.idx].size == 0 {
            self.idx += 1;
        }
    }

    /// Store one chunk. Returns the ack to send and any files that just landed in the destination.
    pub fn on_chunk(&mut self, file: u16, data: &[u8]) -> Result<(Msg, Vec<PathBuf>), String> {
        if file as usize != self.idx || self.idx >= self.files.len() {
            return Err(format!("檔案片段順序錯誤：收到第 {file} 個，預期第 {} 個", self.idx));
        }
        let size = self.files[self.idx].size;
        if self.got_in_file + data.len() as u64 > size {
            return Err(format!("{} 收到的資料比宣告的 {size} bytes 多", self.files[self.idx].name));
        }
        if self.open.is_none() {
            self.open = Some(File::create(self.staged(self.idx)).map_err(|e| format!("無法寫入暫存檔：{e}"))?);
        }
        self.open.as_mut().expect("opened above").write_all(data).map_err(|e| format!("寫入暫存檔失敗：{e}"))?;
        self.got_in_file += data.len() as u64;
        self.got += data.len() as u64;
        if self.got_in_file == size {
            self.open = None;
            self.got_in_file = 0;
            self.idx += 1;
            self.skip_empty();
        }
        let landed = self.settle()?;
        Ok((Msg::DragAck { drag: self.drag, bytes: self.got }, landed))
    }

    /// The user let go here: every finished file moves to the destination now, the rest as they finish.
    pub fn drop_here(&mut self) -> Result<Vec<PathBuf>, String> {
        self.dropped = true;
        self.settle()
    }

    fn settle(&mut self) -> Result<Vec<PathBuf>, String> {
        let mut out = Vec::new();
        if !self.dropped {
            return Ok(out);
        }
        for i in 0..self.files.len() {
            let complete = i < self.idx || (self.files[i].size == 0);
            if complete && self.landed[i].is_none() {
                let p = land(&self.staged(i), &self.dest, &self.files[i].name)?;
                self.landed[i] = Some(p.clone());
                out.push(p);
            }
        }
        Ok(out)
    }

    pub fn dropped(&self) -> bool {
        self.dropped
    }

    /// Dropped and every file is in the destination.
    pub fn done(&self) -> bool {
        self.dropped && self.landed.iter().all(Option::is_some)
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        self.open = None;
        if let Err(e) = fs::remove_dir_all(&self.dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("flowseam files: could not remove staging {}: {e}", self.dir.display());
            }
        }
    }
}

enum Ctl {
    Ack(u64),
    Cancel,
}

/// Feed one drag to `send` until it is acknowledged, cancelled, stalled, or `send` fails. Returns why it ended.
fn run_send(mut out: Outgoing, ctl: Receiver<Ctl>, send: &dyn Fn(Msg) -> bool) -> String {
    let drag = out.drag;
    loop {
        loop {
            match out.next_chunk() {
                Ok(Some(m)) => {
                    if !send(m) {
                        return "連線已中斷".into();
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    send(Msg::DragCancel { drag });
                    return e;
                }
            }
        }
        if out.all_acked() {
            return "完成".into();
        }
        match ctl.recv_timeout(STALL) {
            Ok(Ctl::Ack(n)) => out.on_ack(n),
            Ok(Ctl::Cancel) => return "已取消".into(),
            Err(RecvTimeoutError::Timeout) => {
                send(Msg::DragCancel { drag });
                return "對方太久沒有回應".into();
            }
            Err(RecvTimeoutError::Disconnected) => return "已取消".into(),
        }
    }
}

pub type SendFn = Arc<dyn Fn(Msg) -> bool + Send + Sync>;

/// Both directions of file drags for one link session.
pub struct DragPump {
    staging: PathBuf,
    dest: PathBuf,
    free: Option<FreeSpace>,
    next_drag: u32,
    incoming: Option<Incoming>,
    outgoing: Option<(u32, Sender<Ctl>)>,
}

impl DragPump {
    /// `staging`: where unfinished files wait (ideally on the same volume as `dest`). `dest`: where a
    /// dropped file lands, the Downloads folder.
    pub fn new(staging: PathBuf, dest: PathBuf) -> Self {
        DragPump { staging, dest, free: None, next_drag: 1, incoming: None, outgoing: None }
    }

    /// Refuse a drag that would leave less than `RESERVE_BYTES` free where it lands.
    pub fn with_free_space(mut self, free: FreeSpace) -> Self {
        self.free = Some(free);
        self
    }

    /// The bytes are bounded by the announced sizes (`Incoming` refuses anything past them), so checking the
    /// announced total up front keeps a large drag from filling the disk before it fails. Unmeasurable space
    /// is reported and the drag received anyway: writing would still fail loudly on a full disk.
    fn room_for(&self, total: u64) -> Result<(), String> {
        let Some(free) = &self.free else { return Ok(()) };
        match free(&self.dest) {
            Ok(avail) if total.saturating_add(RESERVE_BYTES) > avail => {
                Err(format!("需要 {}，{} 只剩 {}（保留 {}）", human(total), self.dest.display(), human(avail), human(RESERVE_BYTES)))
            }
            Ok(_) => Ok(()),
            Err(e) => {
                eprintln!("flowseam files: free space unknown ({e}); receiving anyway");
                Ok(())
            }
        }
    }

    /// A file drag left this machine for the peer: announce it and start streaming. Replaces any drag
    /// still being sent.
    pub fn start(&mut self, paths: &[PathBuf], send: SendFn) {
        self.cancel_outgoing(&*send);
        let drag = self.next_drag;
        self.next_drag = self.next_drag.wrapping_add(1).max(1);
        let (out, begin, skipped) = match Outgoing::new(drag, paths) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("flowseam files: drag not sent: {e}");
                return;
            }
        };
        for s in skipped {
            eprintln!("flowseam files: skipped {s}");
        }
        if !send(begin) {
            return;
        }
        let (tx, rx) = channel();
        self.outgoing = Some((drag, tx));
        std::thread::spawn(move || {
            let why = run_send(out, rx, &*send);
            eprintln!("flowseam files: drag {drag} sent: {why}");
        });
    }

    fn cancel_outgoing(&mut self, send: &dyn Fn(Msg) -> bool) {
        if let Some((drag, tx)) = self.outgoing.take() {
            if tx.send(Ctl::Cancel).is_ok() {
                send(Msg::DragCancel { drag });
            }
        }
    }

    /// A drag message from the peer. Returns the files that landed in the destination because of it.
    pub fn on_msg(&mut self, msg: Msg, send: &dyn Fn(Msg) -> bool) -> Vec<PathBuf> {
        match msg {
            Msg::DragBegin { drag, files } => {
                self.incoming = None;
                let total = files.iter().fold(0u64, |t, f| t.saturating_add(f.size));
                if let Err(e) = self.room_for(total) {
                    eprintln!("flowseam files: refused drag {drag}: {e}");
                    send(Msg::DragCancel { drag });
                    return Vec::new();
                }
                match Incoming::begin(&self.staging, &self.dest, drag, &files) {
                    Ok(inc) => self.incoming = Some(inc),
                    Err(e) => {
                        eprintln!("flowseam files: cannot receive drag {drag}: {e}");
                        send(Msg::DragCancel { drag });
                    }
                }
                Vec::new()
            }
            Msg::FileChunk { drag, file, data } => {
                let Some(inc) = self.incoming.as_mut().filter(|i| i.drag() == drag) else { return Vec::new() };
                match inc.on_chunk(file, &data) {
                    Ok((ack, landed)) => {
                        send(ack);
                        self.finish_if_done();
                        landed
                    }
                    Err(e) => {
                        eprintln!("flowseam files: drag {drag} failed: {e}");
                        self.incoming = None;
                        send(Msg::DragCancel { drag });
                        Vec::new()
                    }
                }
            }
            Msg::DragAck { drag, bytes } => {
                if let Some((d, tx)) = &self.outgoing {
                    if *d == drag && tx.send(Ctl::Ack(bytes)).is_err() {
                        self.outgoing = None;
                    }
                }
                Vec::new()
            }
            Msg::DragCancel { drag } => {
                if self.incoming.as_ref().is_some_and(|i| i.drag() == drag) {
                    eprintln!("flowseam files: peer cancelled drag {drag}");
                    self.incoming = None;
                }
                if self.outgoing.as_ref().is_some_and(|(d, _)| *d == drag) {
                    if let Some((_, tx)) = self.outgoing.take() {
                        let _ = tx.send(Ctl::Cancel);
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The primary button came up on this machine: a drag received from the peer is dropped here.
    pub fn on_primary_up(&mut self, send: &dyn Fn(Msg) -> bool) -> Vec<PathBuf> {
        let Some(inc) = self.incoming.as_mut().filter(|i| !i.dropped()) else { return Vec::new() };
        let drag = inc.drag();
        match inc.drop_here() {
            Ok(landed) => {
                self.finish_if_done();
                landed
            }
            Err(e) => {
                eprintln!("flowseam files: drop of drag {drag} failed: {e}");
                self.incoming = None;
                send(Msg::DragCancel { drag });
                Vec::new()
            }
        }
    }

    /// The cursor left this machine with the button still down: a received drag that was not dropped goes back.
    pub fn on_cross_away(&mut self, send: &dyn Fn(Msg) -> bool) {
        if let Some(inc) = self.incoming.as_ref().filter(|i| !i.dropped()) {
            let drag = inc.drag();
            self.incoming = None;
            send(Msg::DragCancel { drag });
        }
    }

    fn finish_if_done(&mut self) {
        if self.incoming.as_ref().is_some_and(Incoming::done) {
            self.incoming = None;
        }
    }
}

impl Drop for DragPump {
    fn drop(&mut self) {
        if let Some((_, tx)) = self.outgoing.take() {
            let _ = tx.send(Ctl::Cancel);
        }
    }
}

/// This machine's own drags, read from the OS (the macOS drag pasteboard).
pub trait DragSource: Send + Sync {
    /// Changes whenever a drag starts on this machine.
    fn generation(&self) -> Result<u64, String>;
    /// The files of the drag in progress.
    fn files(&self) -> Result<Vec<PathBuf>, String>;
}

type Links = Arc<std::sync::Mutex<Vec<(String, Arc<std::sync::Mutex<DragPump>>, SendFn)>>>;

enum Job {
    Up,
    Away { peer: String, send: bool },
    Flush(Sender<()>),
}

/// Every live link's `DragPump` and this machine's drag source, behind the forwarder's two drag hooks.
///
/// The hooks run under the forwarder's lock, on the event tap's thread on a Mac, where blocking stalls
/// every key and mouse event and a slow tap gets disabled by the OS. So a hook only takes the one decision
/// that must be made then (did a drag start during this press, to cancel it here); reading the file list,
/// statting files, writing and moving staged data all happen on the hub's own thread, in hook order.
/// Lock order there: the link list, then a pump. Nothing that holds a pump takes the list.
pub struct DragHub {
    source: Option<Arc<dyn DragSource>>,
    /// Drag generation when the primary button went down; a drag started during this press if it moved.
    baseline: Option<u64>,
    links: Links,
    jobs: Sender<Job>,
}

impl DragHub {
    /// `source: None` on a machine that cannot start drags yet: it still receives them.
    pub fn new(source: Option<Box<dyn DragSource>>) -> Self {
        let source: Option<Arc<dyn DragSource>> = source.map(Arc::from);
        let links: Links = Arc::default();
        let (jobs, rx) = channel();
        let (l, src) = (links.clone(), source.clone());
        std::thread::Builder::new()
            .name("flowseam-drags".into())
            .spawn(move || run_jobs(rx, &l, src.as_deref()))
            .expect("spawn drag thread");
        DragHub { source, baseline: None, links, jobs }
    }

    pub fn attach(&mut self, peer: &str, pump: Arc<std::sync::Mutex<DragPump>>, send: SendFn) {
        let mut l = self.links.lock().unwrap_or_else(|e| e.into_inner());
        l.retain(|(p, ..)| p != peer);
        l.push((peer.to_string(), pump, send));
    }

    /// Forget `peer`'s pump if it is still this one (a newer session may have replaced it).
    pub fn detach(&mut self, peer: &str, pump: &Arc<std::sync::Mutex<DragPump>>) {
        self.links.lock().unwrap_or_else(|e| e.into_inner()).retain(|(p, q, _)| !(p == peer && Arc::ptr_eq(q, pump)));
    }

    /// The primary button changed here. A press reads the drag generation (one pasteboard call); a release
    /// drops what a peer dragged here, on the hub's thread.
    pub fn primary(&mut self, down: bool) {
        if down {
            self.baseline = match self.source.as_ref().map(|s| s.generation()) {
                Some(Ok(g)) => Some(g),
                Some(Err(e)) => {
                    eprintln!("flowseam files: drag source unreadable: {e}");
                    None
                }
                None => None,
            };
        } else {
            self.baseline = None;
            let _ = self.jobs.send(Job::Up);
        }
    }

    /// A drag started here during the current press (read now, nothing consumed). False with no press
    /// recorded or no drag source, so a held press made here keeps the cursor here.
    pub fn drag_started(&self) -> bool {
        match (self.baseline, self.source.as_ref()) {
            (Some(base), Some(src)) => src.generation().is_ok_and(|g| g != base),
            _ => false,
        }
    }

    /// The cursor leaves for `peer`. A drag received here and not dropped goes back; a drag that started
    /// here during this press is sent to `peer`. True when a drag started during this press, so the caller
    /// cancels it here instead of letting it drop at the edge.
    pub fn drag_away(&mut self, peer: &str) -> bool {
        let started = match (self.baseline.take(), self.source.as_ref()) {
            (Some(base), Some(src)) => match src.generation() {
                Ok(g) => g != base, // unchanged: a window was dragged, not a new drag
                Err(e) => {
                    eprintln!("flowseam files: drag source unreadable: {e}");
                    false
                }
            },
            _ => false,
        };
        let linked = self.links.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(p, ..)| p == peer);
        if started && !linked {
            eprintln!("flowseam files: no live link to {peer}; drag not sent");
        }
        let _ = self.jobs.send(Job::Away { peer: peer.to_string(), send: started && linked });
        started && linked
    }

    /// Wait until every hook so far has been handled (tests, and anyone who needs the files settled).
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = channel();
        self.jobs.send(Job::Flush(tx)).is_ok() && rx.recv_timeout(timeout).is_ok()
    }
}

fn run_jobs(rx: Receiver<Job>, links: &Links, source: Option<&dyn DragSource>) {
    while let Ok(job) = rx.recv() {
        let all = links.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match job {
            Job::Up => {
                for (_, pump, send) in &all {
                    for p in lock(pump).on_primary_up(&**send) {
                        eprintln!("flowseam files: saved {}", p.display());
                    }
                }
            }
            Job::Away { peer, send } => {
                for (_, pump, s) in &all {
                    lock(pump).on_cross_away(&**s);
                }
                if send {
                    start_drag(&all, &peer, source);
                }
            }
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn start_drag(all: &[(String, Arc<std::sync::Mutex<DragPump>>, SendFn)], peer: &str, source: Option<&dyn DragSource>) {
    let Some(src) = source else { return };
    let files = match src.files() {
        Ok(f) if !f.is_empty() => f,
        Ok(_) => return, // text or an image was dragged, not files
        Err(e) => {
            eprintln!("flowseam files: dragged files unreadable: {e}");
            return;
        }
    };
    match all.iter().find(|(p, ..)| p == peer) {
        Some((_, pump, send)) => lock(pump).start(&files, send.clone()),
        None => eprintln!("flowseam files: no live link to {peer}; drag not sent"),
    }
}

fn lock(p: &std::sync::Mutex<DragPump>) -> std::sync::MutexGuard<'_, DragPump> {
    p.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let p = std::env::temp_dir().join(format!("flowseam-files-{tag}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn sub(&self, s: &str) -> PathBuf {
            let p = self.0.join(s);
            fs::create_dir_all(&p).unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn peer_names_become_one_safe_component() {
        assert_eq!(safe_name("報告.pdf"), "報告.pdf");
        assert_eq!(safe_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(safe_name("..\\..\\Windows\\x.dll"), ".._.._Windows_x.dll");
        assert_eq!(safe_name("a:b*c?.txt"), "a_b_c_.txt");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name("name. . "), "name");
        assert_eq!(safe_name("line\nbreak"), "line_break");
        assert_eq!(safe_name("CON"), "_CON");
        assert_eq!(safe_name("con.tar.gz"), "_con.tar.gz");
        assert_eq!(safe_name("LPT9.txt"), "_LPT9.txt");
        assert_eq!(safe_name("COM0.txt"), "COM0.txt");
        assert_eq!(safe_name("CONSOLE.txt"), "CONSOLE.txt");
        let long = "字".repeat(100); // 300 bytes
        let s = safe_name(&long);
        assert!(s.len() <= MAX_FILE_NAME_BYTES && s.chars().all(|c| c == '字'), "{}", s.len());
    }

    #[test]
    fn claimed_names_never_collide() {
        let t = Tmp::new("claim");
        let a = claim_unique(&t.0, "a.txt").unwrap();
        let b = claim_unique(&t.0, "a.txt").unwrap();
        let c = claim_unique(&t.0, ".bashrc").unwrap();
        let d = claim_unique(&t.0, ".bashrc").unwrap();
        assert_eq!(a.file_name().unwrap(), "a.txt");
        assert_eq!(b.file_name().unwrap(), "a (1).txt");
        assert_eq!(c.file_name().unwrap(), ".bashrc");
        assert_eq!(d.file_name().unwrap(), ".bashrc (1)");
        let long = format!("{}.txt", "x".repeat(MAX_FILE_NAME_BYTES - 4));
        claim_unique(&t.0, &long).unwrap();
        let second = claim_unique(&t.0, &long).unwrap();
        let n = second.file_name().unwrap().to_string_lossy().into_owned();
        assert!(n.len() <= MAX_FILE_NAME_BYTES && n.ends_with(" (1).txt"), "{n}");
    }

    /// Runs a whole drag through Outgoing and Incoming as the link would carry it.
    fn transfer(src: &[PathBuf], staging: &Path, dest: &Path, drop_at_chunk: Option<usize>) -> (Incoming, Vec<PathBuf>) {
        let (mut out, begin, _) = Outgoing::new(9, src).unwrap();
        let Msg::DragBegin { drag, files } = begin else { panic!() };
        let mut inc = Incoming::begin(staging, dest, drag, &files).unwrap();
        let mut landed = Vec::new();
        let mut chunks = 0;
        if drop_at_chunk == Some(0) {
            landed.extend(inc.drop_here().unwrap());
        }
        loop {
            let mut progressed = false;
            while let Some(Msg::FileChunk { file, data, .. }) = out.next_chunk().unwrap() {
                let (ack, l) = inc.on_chunk(file, &data).unwrap();
                landed.extend(l);
                chunks += 1;
                if drop_at_chunk == Some(chunks) {
                    landed.extend(inc.drop_here().unwrap());
                }
                let Msg::DragAck { bytes, .. } = ack else { panic!() };
                out.on_ack(bytes);
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
        assert!(out.all_acked());
        (inc, landed)
    }

    #[test]
    fn dropped_drag_lands_in_downloads() {
        let t = Tmp::new("drop");
        let (src, staging, dest) = (t.sub("src"), t.sub("staging"), t.sub("dest"));
        let big: Vec<u8> = (0..(MAX_CHUNK_BYTES * 5 + 123)).map(|i| (i * 7 % 251) as u8).collect();
        fs::write(src.join("big.bin"), &big).unwrap();
        fs::write(src.join("empty.txt"), b"").unwrap();
        fs::write(src.join("筆記.md"), "嗨").unwrap();
        fs::write(dest.join("筆記.md"), "already here").unwrap();
        let paths = vec![src.join("big.bin"), src.join("empty.txt"), src.join("筆記.md")];
        let (mut inc, mut landed) = transfer(&paths, &staging, &dest, None);
        assert!(landed.is_empty(), "nothing lands before the drop");
        assert_eq!(entries(&dest), vec!["筆記.md"]);
        landed.extend(inc.drop_here().unwrap());
        assert!(inc.done());
        assert_eq!(landed.len(), 3);
        assert_eq!(fs::read(dest.join("big.bin")).unwrap(), big);
        assert_eq!(fs::read(dest.join("empty.txt")).unwrap(), b"");
        assert_eq!(fs::read_to_string(dest.join("筆記 (1).md")).unwrap(), "嗨");
        assert_eq!(fs::read_to_string(dest.join("筆記.md")).unwrap(), "already here", "an existing file is never overwritten");
        drop(inc);
        assert!(entries(&staging).is_empty(), "staging is cleaned up");
    }

    #[test]
    fn drop_before_the_data_lands_each_file_as_it_finishes() {
        let t = Tmp::new("early");
        let (src, staging, dest) = (t.sub("src"), t.sub("staging"), t.sub("dest"));
        fs::write(src.join("a"), vec![1u8; MAX_CHUNK_BYTES * 2]).unwrap();
        fs::write(src.join("b"), vec![2u8; 10]).unwrap();
        let (inc, landed) = transfer(&[src.join("a"), src.join("b")], &staging, &dest, Some(1));
        assert!(inc.done());
        assert_eq!(landed.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(fs::read(dest.join("a")).unwrap().len(), MAX_CHUNK_BYTES * 2);
    }

    #[test]
    fn a_drag_that_is_never_dropped_leaves_nothing_behind() {
        let t = Tmp::new("undropped");
        let (src, staging, dest) = (t.sub("src"), t.sub("staging"), t.sub("dest"));
        fs::write(src.join("a"), vec![1u8; 1000]).unwrap();
        let (inc, landed) = transfer(&[src.join("a")], &staging, &dest, None);
        assert!(landed.is_empty());
        drop(inc);
        assert!(entries(&dest).is_empty());
        assert!(entries(&staging).is_empty());
    }

    #[test]
    fn sender_never_runs_more_than_the_window_ahead() {
        let t = Tmp::new("window");
        let src = t.sub("src");
        fs::write(src.join("a"), vec![0u8; (WINDOW_BYTES * 3) as usize]).unwrap();
        let (mut out, _, _) = Outgoing::new(1, &[src.join("a")]).unwrap();
        let mut sent = 0u64;
        while let Some(Msg::FileChunk { data, .. }) = out.next_chunk().unwrap() {
            sent += data.len() as u64;
        }
        assert_eq!(sent, WINDOW_BYTES, "stops at the window with nothing acked");
        out.on_ack(MAX_CHUNK_BYTES as u64);
        assert!(out.next_chunk().unwrap().is_some(), "one acked chunk opens room for one more");
        assert!(out.next_chunk().unwrap().is_none());
    }

    #[test]
    fn an_ack_for_bytes_never_sent_does_not_open_the_window() {
        let t = Tmp::new("liar");
        let src = t.sub("src");
        fs::write(src.join("a"), vec![0u8; (WINDOW_BYTES * 3) as usize]).unwrap();
        let (mut out, _, _) = Outgoing::new(1, &[src.join("a")]).unwrap();
        out.on_ack(u64::MAX);
        let mut sent = 0u64;
        while let Some(Msg::FileChunk { data, .. }) = out.next_chunk().unwrap() {
            sent += data.len() as u64;
        }
        assert_eq!(sent, WINDOW_BYTES);
        assert!(!out.all_acked());
    }

    #[test]
    fn hostile_chunks_are_refused() {
        let t = Tmp::new("hostile");
        let (staging, dest) = (t.sub("staging"), t.sub("dest"));
        let files = vec![FileMeta { name: "a".into(), size: 4 }, FileMeta { name: "b".into(), size: 4 }];
        let mut inc = Incoming::begin(&staging, &dest, 1, &files).unwrap();
        assert!(inc.on_chunk(1, b"xx").is_err(), "out of order");
        let mut inc = Incoming::begin(&staging, &dest, 1, &files).unwrap();
        assert!(inc.on_chunk(0, b"12345").is_err(), "more than announced");
        let mut inc = Incoming::begin(&staging, &dest, 1, &files).unwrap();
        inc.on_chunk(0, b"1234").unwrap();
        inc.on_chunk(1, b"1234").unwrap();
        assert!(inc.on_chunk(1, b"1").is_err(), "past the last file");
        assert!(inc.on_chunk(2, b"1").is_err(), "no such file");
    }

    #[test]
    fn a_file_that_shrinks_mid_drag_is_an_error() {
        let t = Tmp::new("shrink");
        let src = t.sub("src");
        fs::write(src.join("a"), vec![0u8; 100]).unwrap();
        let (mut out, _, _) = Outgoing::new(1, &[src.join("a")]).unwrap();
        fs::write(src.join("a"), vec![0u8; 10]).unwrap();
        assert!(out.next_chunk().is_err());
    }

    #[test]
    fn folders_are_skipped_and_named() {
        let t = Tmp::new("folder");
        let src = t.sub("src");
        let dir = t.sub("src/inner");
        fs::write(src.join("a"), b"x").unwrap();
        let (_, begin, skipped) = Outgoing::new(1, &[dir.clone(), src.join("a")]).unwrap();
        let Msg::DragBegin { files, .. } = begin else { panic!() };
        assert_eq!(files.len(), 1);
        assert_eq!(skipped.len(), 1);
        assert!(Outgoing::new(1, &[dir]).is_err(), "a drag of only folders sends nothing");
    }

    struct FakeSource(Arc<Mutex<(u64, Vec<PathBuf>)>>);
    impl DragSource for FakeSource {
        fn generation(&self) -> Result<u64, String> {
            Ok(self.0.lock().unwrap().0)
        }
        fn files(&self) -> Result<Vec<PathBuf>, String> {
            Ok(self.0.lock().unwrap().1.clone())
        }
    }

    type SourceState = Arc<Mutex<(u64, Vec<PathBuf>)>>;
    type Sent = Arc<Mutex<Vec<Msg>>>;

    fn hub_with(t: &Tmp, file: &Path) -> (DragHub, SourceState, Sent, Arc<Mutex<DragPump>>) {
        let state = Arc::new(Mutex::new((5, vec![file.to_path_buf()])));
        let mut hub = DragHub::new(Some(Box::new(FakeSource(state.clone()))));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let s = sent.clone();
        let pump = Arc::new(Mutex::new(DragPump::new(t.sub("staging"), t.sub("dest"))));
        hub.attach("peer", pump.clone(), Arc::new(move |m| { s.lock().unwrap().push(m); true }));
        (hub, state, sent, pump)
    }

    fn began(sent: &Mutex<Vec<Msg>>) -> bool {
        sent.lock().unwrap().iter().any(|m| matches!(m, Msg::DragBegin { .. }))
    }

    #[test]
    fn only_a_drag_started_during_this_press_leaves() {
        let t = Tmp::new("hub");
        let src = t.sub("src");
        fs::write(src.join("a"), b"x").unwrap();
        let (mut hub, state, sent, _) = hub_with(&t, &src.join("a"));
        // A window dragged to the edge: the drag pasteboard still holds an old drag.
        hub.primary(true);
        assert!(!hub.drag_away("peer"));
        assert!(!began(&sent));
        // A new file drag during the press.
        hub.primary(true);
        assert!(!hub.drag_started());
        state.lock().unwrap().0 += 1;
        assert!(hub.drag_started());
        assert!(hub.drag_away("peer"));
        assert!(hub.flush(Duration::from_secs(2)));
        assert!(began(&sent));
        // The press was consumed by that crossing: crossing again (back and out) sends nothing more.
        assert!(hub.flush(Duration::from_secs(2)));
        sent.lock().unwrap().clear();
        assert!(!hub.drag_away("peer"));
        assert!(hub.flush(Duration::from_secs(2)));
        assert!(!began(&sent));
        // No press at all: nothing.
        hub.primary(false);
        state.lock().unwrap().0 += 1;
        assert!(!hub.drag_away("peer"));
        // A link that is not up gets nothing.
        hub.primary(true);
        state.lock().unwrap().0 += 1;
        assert!(!hub.drag_away("elsewhere"));
    }

    #[test]
    fn a_release_drops_what_a_peer_dragged_here() {
        let t = Tmp::new("hubdrop");
        let src = t.sub("src");
        fs::write(src.join("a"), b"x").unwrap();
        let (mut hub, _, sent, pump) = hub_with(&t, &src.join("a"));
        let noop = |_m: Msg| true;
        pump.lock().unwrap().on_msg(Msg::DragBegin { drag: 3, files: vec![FileMeta { name: "got.txt".into(), size: 2 }] }, &noop);
        pump.lock().unwrap().on_msg(Msg::FileChunk { drag: 3, file: 0, data: b"hi".to_vec() }, &noop);
        hub.primary(false);
        assert!(hub.flush(Duration::from_secs(2)));
        assert_eq!(fs::read(t.0.join("dest/got.txt")).unwrap(), b"hi");
        // A received drag carried back out instead is cancelled toward its sender.
        pump.lock().unwrap().on_msg(Msg::DragBegin { drag: 4, files: vec![FileMeta { name: "no.txt".into(), size: 2 }] }, &noop);
        hub.drag_away("peer");
        assert!(hub.flush(Duration::from_secs(2)));
        assert!(sent.lock().unwrap().contains(&Msg::DragCancel { drag: 4 }));
        assert!(entries(&t.0.join("staging")).is_empty());
    }

    /// The hooks run on the event tap's thread: a slow file list (a network volume, hundreds of items)
    /// must not hold up the crossing.
    #[test]
    fn a_crossing_never_waits_on_the_file_list() {
        struct Slow(Mutex<u64>);
        impl DragSource for Slow {
            fn generation(&self) -> Result<u64, String> {
                Ok(*self.0.lock().unwrap())
            }
            fn files(&self) -> Result<Vec<PathBuf>, String> {
                std::thread::sleep(Duration::from_millis(800));
                Ok(Vec::new())
            }
        }
        let t = Tmp::new("slow");
        let src = Arc::new(Slow(Mutex::new(1)));
        struct Shared(Arc<Slow>);
        impl DragSource for Shared {
            fn generation(&self) -> Result<u64, String> {
                self.0.generation()
            }
            fn files(&self) -> Result<Vec<PathBuf>, String> {
                self.0.files()
            }
        }
        let mut hub = DragHub::new(Some(Box::new(Shared(src.clone()))));
        hub.attach("peer", Arc::new(Mutex::new(DragPump::new(t.sub("s"), t.sub("d")))), Arc::new(|_| true));
        hub.primary(true);
        *src.0.lock().unwrap() += 1;
        let t0 = std::time::Instant::now();
        assert!(hub.drag_away("peer"));
        assert!(t0.elapsed() < Duration::from_millis(300), "crossing waited {:?}", t0.elapsed());
        assert!(hub.flush(Duration::from_secs(3)));
    }

    #[test]
    fn a_drag_bigger_than_the_free_space_is_refused_before_anything_is_written() {
        let t = Tmp::new("full");
        let (staging, dest) = (t.sub("staging"), t.sub("dest"));
        let sent = Mutex::new(Vec::new());
        let send = |m: Msg| { sent.lock().unwrap().push(m); true };
        let begin = |size| Msg::DragBegin { drag: 2, files: vec![FileMeta { name: "big".into(), size }] };
        let mut pump = DragPump::new(staging.clone(), dest.clone()).with_free_space(Arc::new(|_| Ok(RESERVE_BYTES + 100)));
        pump.on_msg(begin(101), &send);
        assert_eq!(*sent.lock().unwrap(), vec![Msg::DragCancel { drag: 2 }]);
        assert!(entries(&staging).is_empty());
        sent.lock().unwrap().clear();
        pump.on_msg(begin(100), &send);
        assert!(sent.lock().unwrap().is_empty(), "exactly fits: accepted");
        // Space that cannot be measured is not a refusal.
        let mut pump = DragPump::new(staging, dest).with_free_space(Arc::new(|_| Err("statvfs failed".into())));
        pump.on_msg(begin(u64::MAX), &send);
        assert!(sent.lock().unwrap().is_empty());
    }

    /// Two pumps joined back to back, as two ends of a link: chunks and acks flow through threads.
    #[test]
    fn pumps_carry_a_drag_end_to_end_and_cancel_cleans_up() {
        let t = Tmp::new("pumps");
        let (src, sa, da, sb, db) = (t.sub("src"), t.sub("sa"), t.sub("da"), t.sub("sb"), t.sub("db"));
        let data: Vec<u8> = (0..(WINDOW_BYTES as usize * 3 + 5)).map(|i| i as u8).collect();
        fs::write(src.join("movie.mov"), &data).unwrap();
        let a = Arc::new(Mutex::new(DragPump::new(sa, da)));
        let b = Arc::new(Mutex::new(DragPump::new(sb.clone(), db.clone())));
        let (to_b, at_b) = channel::<Msg>();
        let (to_a, at_a) = channel::<Msg>();
        let send_ab: SendFn = Arc::new(move |m| to_b.send(m).is_ok());
        let send_ba: SendFn = { let to_a = to_a.clone(); Arc::new(move |m| to_a.send(m).is_ok()) };
        // Router threads, as the link's inbound threads would be.
        let (bb, sba) = (b.clone(), send_ba.clone());
        let rb = std::thread::spawn(move || {
            while let Ok(m) = at_b.recv_timeout(Duration::from_secs(2)) {
                bb.lock().unwrap().on_msg(m, &*sba);
            }
        });
        let (aa, sab) = (a.clone(), send_ab.clone());
        let ra = std::thread::spawn(move || {
            while let Ok(m) = at_a.recv_timeout(Duration::from_secs(2)) {
                aa.lock().unwrap().on_msg(m, &*sab);
            }
        });
        a.lock().unwrap().start(&[src.join("movie.mov")], send_ab.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !entries(&sb).iter().any(|_| true) {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));
        assert!(entries(&db).is_empty(), "streamed but not yet dropped");
        let landed = b.lock().unwrap().on_primary_up(&*send_ba);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && fs::read(db.join("movie.mov")).map(|d| d.len()).unwrap_or(0) != data.len() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs::read(db.join("movie.mov")).unwrap(), data, "landed {landed:?}");
        // A second drag that comes back before the drop: staged data is deleted, nothing lands.
        fs::write(src.join("second.txt"), b"second").unwrap();
        a.lock().unwrap().start(&[src.join("second.txt")], send_ab.clone());
        std::thread::sleep(Duration::from_millis(300));
        b.lock().unwrap().on_cross_away(&*send_ba);
        std::thread::sleep(Duration::from_millis(100));
        assert!(entries(&sb).is_empty(), "staging emptied: {:?}", entries(&sb));
        assert_eq!(entries(&db), vec!["movie.mov"]);
        drop(send_ab);
        drop(send_ba);
        drop(to_a);
        drop((a, b));
        let _ = (ra.join(), rb.join());
    }
}
