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

/// One incoming drag's files in staging, shared with a native drop session whose callbacks run on the OS's own
/// threads. The pump writes and reports progress; a session waits for a file and takes it, or reads it as it grows.
/// Nothing here touches a pump, so a target waiting on a slow transfer holds up no input. The staging folder goes
/// when the last holder lets go.
pub struct Staged {
    dir: PathBuf,
    files: Vec<FileMeta>,
    state: std::sync::Mutex<Progress>,
    changed: std::sync::Condvar,
}

struct Progress {
    /// Bytes of each file on disk.
    have: Vec<u64>,
    /// Why no more bytes will come (cancelled, failed, the link went), once that is so.
    over: Option<String>,
}

impl Staged {
    fn new(dir: PathBuf, files: Vec<FileMeta>) -> Arc<Staged> {
        let progress = Progress { have: vec![0; files.len()], over: None };
        Arc::new(Staged { dir, files, state: std::sync::Mutex::new(progress), changed: std::sync::Condvar::new() })
    }

    pub fn files(&self) -> &[FileMeta] {
        &self.files
    }

    pub fn path(&self, i: usize) -> PathBuf {
        self.dir.join(format!("{i}.part"))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Progress> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn grew(&self, i: usize, have: u64) {
        self.lock().have[i] = have;
        self.changed.notify_all();
    }

    fn end(&self, why: &str) {
        let mut p = self.lock();
        if p.over.is_none() {
            p.over = Some(why.to_string());
        }
        drop(p);
        self.changed.notify_all();
    }

    /// Block until file `i` has more than `at` bytes on disk, or all of them; returns how many it has. An error once
    /// the drag is over and file `i` will never be complete.
    pub fn wait_past(&self, i: usize, at: u64) -> Result<u64, String> {
        let size = self.files.get(i).ok_or_else(|| format!("no file {i} in this drag"))?.size;
        let mut p = self.lock();
        loop {
            if p.have[i] > at || p.have[i] == size {
                return Ok(p.have[i]);
            }
            if let Some(why) = &p.over {
                return Err(why.clone());
            }
            p = self.changed.wait(p).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Block until file `i` is complete, then move it to exactly `to` (where a drop target promised it would be).
    pub fn deliver(&self, i: usize, to: &Path) -> Result<(), String> {
        let size = self.files.get(i).ok_or_else(|| format!("no file {i} in this drag"))?.size;
        while self.wait_past(i, size.saturating_sub(1))? < size {}
        let from = self.path(i);
        if fs::rename(&from, to).is_err() {
            fs::copy(&from, to).map_err(|e| format!("無法存到 {}：{e}", to.display()))?;
            let _ = fs::remove_file(&from);
        }
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("flowseam files: could not remove staging {}: {e}", self.dir.display());
            }
        }
    }
}

/// Receiving side of one drag: files staged under their own folder, moved out on a drop.
pub struct Incoming {
    drag: u32,
    staged: Arc<Staged>,
    dest: PathBuf,
    files: Vec<FileMeta>,
    /// File the next chunk must belong to.
    idx: usize,
    open: Option<File>,
    got_in_file: u64,
    got: u64,
    dropped: bool,
    /// A native drop target took the files: it reads them through `staged`, nothing lands in `dest`.
    native: bool,
    landed: Vec<Option<PathBuf>>,
}

impl Incoming {
    pub fn begin(staging_root: &Path, dest: &Path, drag: u32, files: &[FileMeta]) -> Result<Incoming, String> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let dir = staging_root.join(format!("{drag}-{nanos}"));
        fs::create_dir_all(&dir).map_err(|e| format!("無法建立暫存資料夾 {}：{e}", dir.display()))?;
        let files: Vec<FileMeta> = files.iter().map(|f| FileMeta { name: safe_name(&f.name), size: f.size }).collect();
        let staged = Staged::new(dir, files.clone());
        let mut inc = Incoming {
            drag,
            staged,
            dest: dest.to_path_buf(),
            landed: vec![None; files.len()],
            files,
            idx: 0,
            open: None,
            got_in_file: 0,
            got: 0,
            dropped: false,
            native: false,
        };
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
        self.staged.path(i)
    }

    /// The staged files, for a native drop target to wait on and take.
    pub fn shared(&self) -> Arc<Staged> {
        self.staged.clone()
    }

    /// A native drop target took the files: from now on they go to it, not to `dest`.
    pub fn hand_off(&mut self) {
        self.dropped = true;
        self.native = true;
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
        self.staged.grew(self.idx, self.got_in_file);
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
        if !self.dropped || self.native {
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

    /// Dropped and every file is in the destination (or, for a native target, every byte is staged for it).
    pub fn done(&self) -> bool {
        match self.native {
            true => self.idx >= self.files.len(),
            false => self.dropped && self.landed.iter().all(Option::is_some),
        }
    }
}

impl Drop for Incoming {
    /// No more bytes will come; a native target still waiting on an unfinished file is told so. The folder itself
    /// goes with the last holder of `staged`.
    fn drop(&mut self) {
        self.open = None;
        self.staged.end("拖曳已結束，檔案沒有傳完");
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
///
/// Sending is two steps, so ordering on the link is right however fast the user is. The hub puts `DragSoon`
/// on the link at the crossing, before any later input (the release above all); `announced` and then
/// `start_announced` record it and stat and stream the files. A recipient that sees the release between
/// `DragSoon` and `DragBegin` holds the drop until `DragBegin` arrives. Only one thread calls into a pump,
/// the hub's, so a write stuck on a slow disk holds up drags and nothing else.
pub struct DragPump {
    staging: PathBuf,
    dest: PathBuf,
    free: Option<FreeSpace>,
    incoming: Option<Incoming>,
    /// Announced by the peer (`DragSoon`) and not yet begun; `true` once dropped here meanwhile.
    pending: Option<(u32, bool)>,
    /// Ours: announced (no sender thread yet) or streaming.
    outgoing: Option<(u32, Option<Sender<Ctl>>)>,
    /// Starts a system drag here for a drag arriving from the peer; without one a drop lands in `dest`.
    native: Option<Arc<dyn NativeDrop>>,
    /// The system drag carrying an incoming drag now, and how it reports its end (set by the hub).
    session: Option<(u32, Box<dyn NativeSession>)>,
    report: Option<NativeReport>,
}

/// A system drag on this machine for files arriving from the peer: a file-promise drag on macOS, a virtual-file
/// drag on Windows. The drop target decides where the files go.
pub trait NativeDrop: Send + Sync {
    /// Start it under the cursor, with the primary button pressed by this call. `ended` must be called exactly once,
    /// from any thread: `true` when a target took the files (it takes them through `staged`), `false` when the drag
    /// ended without one or never got going.
    fn begin(&self, drag: u32, staged: Arc<Staged>, ended: Box<dyn FnOnce(bool) + Send>) -> Result<Box<dyn NativeSession>, String>;
}

/// One running system drag.
pub trait NativeSession: Send {
    /// The drag is no longer ours to drop (carried back, or the sender cancelled): refuse any drop from now on and
    /// end the system drag. Called on the hub's thread; must not block.
    fn cancel(&self);
}

/// How a pump's system drag reports its end: (drag, a target took the files).
pub type NativeReport = Arc<dyn Fn(u32, bool) + Send + Sync>;

impl DragPump {
    pub fn busy(&self) -> bool { self.incoming.is_some() || self.pending.is_some() || self.outgoing.is_some() || self.session.is_some() }
    /// `staging`: where unfinished files wait (ideally on the same volume as `dest`). `dest`: where a
    /// dropped file lands, the Downloads folder.
    pub fn new(staging: PathBuf, dest: PathBuf) -> Self {
        DragPump { staging, dest, free: None, incoming: None, pending: None, outgoing: None, native: None, session: None, report: None }
    }

    /// Carry each drag that arrives on as a system drag here, so it can be dropped anywhere a file can.
    pub fn with_native(mut self, native: Arc<dyn NativeDrop>) -> Self {
        self.native = Some(native);
        self
    }

    fn end_session(&mut self, why: &str) {
        if let Some((drag, s)) = self.session.take() {
            flowseam_core::log::line(&format!("flowseam files: ending the system drag for drag {drag} ({why})"));
            s.cancel();
        }
    }

    /// Hand the arriving drag to a system drag here. Without one, or if it cannot start, a release here drops the
    /// files into `dest` as before.
    fn start_native(&mut self, drag: u32) {
        let (Some(native), Some(report), Some(inc)) = (self.native.clone(), self.report.clone(), self.incoming.as_ref()) else { return };
        match native.begin(drag, inc.shared(), Box::new(move |took| report(drag, took))) {
            Ok(s) => self.session = Some((drag, s)),
            Err(e) => eprintln!("flowseam files: no system drag for drag {drag} ({e}); a release here saves to Downloads"),
        }
    }

    /// The system drag for `drag` ended. A target that took the files gets them as they finish; with no target the
    /// files land in `dest`, as a plain release would have done.
    pub fn on_native_end(&mut self, drag: u32, took: bool, send: &dyn Fn(Msg) -> bool) -> Vec<PathBuf> {
        if !self.session.as_ref().is_some_and(|(d, _)| *d == drag) {
            return Vec::new(); // cancelled meanwhile, or replaced by a newer drag
        }
        self.session = None;
        let Some(inc) = self.incoming.as_mut().filter(|i| i.drag() == drag) else { return Vec::new() };
        if took {
            inc.hand_off();
            self.finish_if_done();
            return Vec::new();
        }
        eprintln!("flowseam files: drag {drag} ended over nothing that takes files; saving to Downloads");
        self.drop_incoming(send)
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

    /// `DragSoon { drag }` is going to the peer: cancel any drag still being sent and expect this one, so a
    /// cancel from the peer that arrives before `start_announced` stops it. Call it before `DragSoon` leaves.
    pub fn announced(&mut self, drag: u32, send: &dyn Fn(Msg) -> bool) {
        self.cancel_outgoing(send);
        self.outgoing = Some((drag, None));
    }

    /// Stat `paths` and stream them as `drag`, unless the peer cancelled it or a newer drag replaced it.
    pub fn start_announced(&mut self, drag: u32, paths: &[PathBuf], send: SendFn) {
        if self.outgoing.as_ref().map(|(d, tx)| (*d, tx.is_none())) != Some((drag, true)) {
            return;
        }
        let (out, begin, skipped) = match Outgoing::new(drag, paths) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("flowseam files: drag not sent: {e}");
                self.outgoing = None;
                send(Msg::DragCancel { drag });
                return;
            }
        };
        for s in skipped {
            eprintln!("flowseam files: skipped {s}");
        }
        if !send(begin) {
            self.outgoing = None;
            return;
        }
        let (tx, rx) = channel();
        self.outgoing = Some((drag, Some(tx)));
        std::thread::spawn(move || {
            let why = run_send(out, rx, &*send);
            eprintln!("flowseam files: drag {drag} sent: {why}");
        });
    }

    /// The whole send in one call, for a caller that owns the pump outright.
    pub fn start(&mut self, drag: u32, paths: &[PathBuf], send: SendFn) {
        self.announced(drag, &*send);
        send(Msg::DragSoon { drag });
        self.start_announced(drag, paths, send);
    }

    fn cancel_outgoing(&mut self, send: &dyn Fn(Msg) -> bool) {
        if let Some((drag, tx)) = self.outgoing.take() {
            if let Some(tx) = tx {
                let _ = tx.send(Ctl::Cancel);
            }
            send(Msg::DragCancel { drag });
        }
    }

    /// A drag message from the peer. Returns the files that landed in the destination because of it.
    pub fn on_msg(&mut self, msg: Msg, send: &dyn Fn(Msg) -> bool) -> Vec<PathBuf> {
        match msg {
            Msg::DragSoon { drag } => {
                self.end_session("another drag is coming");
                self.incoming = None;
                self.pending = Some((drag, false));
                Vec::new()
            }
            Msg::DragBegin { drag, files } => {
                let dropped = match self.pending.take() {
                    Some((d, dropped)) if d == drag => dropped,
                    // Not announced, or cancelled here already (the cursor came back first).
                    _ => {
                        send(Msg::DragCancel { drag });
                        return Vec::new();
                    }
                };
                self.incoming = None;
                let total = files.iter().fold(0u64, |t, f| t.saturating_add(f.size));
                if let Err(e) = self.room_for(total) {
                    eprintln!("flowseam files: refused drag {drag}: {e}");
                    send(Msg::DragCancel { drag });
                    return Vec::new();
                }
                match Incoming::begin(&self.staging, &self.dest, drag, &files) {
                    Ok(inc) => {
                        self.incoming = Some(inc);
                        // Let go before the data was announced: the drop already happened here.
                        if dropped {
                            return self.drop_incoming(send);
                        }
                        self.start_native(drag);
                    }
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
                if let Some((d, Some(tx))) = &self.outgoing {
                    if *d == drag && tx.send(Ctl::Ack(bytes)).is_err() {
                        self.outgoing = None;
                    }
                }
                Vec::new()
            }
            Msg::DragCancel { drag } => {
                if self.incoming.as_ref().is_some_and(|i| i.drag() == drag) {
                    eprintln!("flowseam files: peer cancelled drag {drag}");
                    self.end_session("peer cancelled");
                    self.incoming = None;
                }
                if self.pending.is_some_and(|(d, _)| d == drag) {
                    self.pending = None;
                }
                if self.outgoing.as_ref().is_some_and(|(d, _)| *d == drag) {
                    if let Some((_, Some(tx))) = self.outgoing.take() {
                        let _ = tx.send(Ctl::Cancel);
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The primary button came up on this machine: a drag received from the peer is dropped here, or, if
    /// only announced so far, dropped the moment it begins.
    pub fn on_primary_up(&mut self, send: &dyn Fn(Msg) -> bool) -> Vec<PathBuf> {
        if self.session.is_some() {
            return Vec::new(); // the release is the system drag's drop; it reports where it went
        }
        if self.incoming.as_ref().is_some_and(|i| !i.dropped()) {
            return self.drop_incoming(send);
        }
        if let Some((_, dropped)) = self.pending.as_mut() {
            *dropped = true;
        }
        Vec::new()
    }

    fn drop_incoming(&mut self, send: &dyn Fn(Msg) -> bool) -> Vec<PathBuf> {
        let Some(inc) = self.incoming.as_mut() else { return Vec::new() };
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

    /// The cursor left this machine with the button still down: a received drag that was not dropped goes
    /// back, whether its data had begun or was only announced.
    pub fn on_cross_away(&mut self, send: &dyn Fn(Msg) -> bool) {
        self.end_session("the cursor left this machine");
        if let Some(inc) = self.incoming.as_ref().filter(|i| !i.dropped()) {
            let drag = inc.drag();
            self.incoming = None;
            send(Msg::DragCancel { drag });
        }
        if let Some((drag, false)) = self.pending {
            self.pending = None;
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
        self.end_session("its link closed");
        if let Some((_, Some(tx))) = self.outgoing.take() {
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

/// One link's pump and how to send on that link, plus the ids its outgoing drags are numbered with. The ids
/// live outside the pump so the crossing can number a drag without taking the pump's lock.
#[derive(Clone)]
pub struct DragLink {
    peer: String,
    pump: Arc<std::sync::Mutex<DragPump>>,
    send: SendFn,
    next_drag: Arc<std::sync::atomic::AtomicU32>,
}

type Links = Arc<std::sync::Mutex<Vec<DragLink>>>;

/// Work for the hub's thread, the only thread that touches a pump. Everything about drags (messages from
/// the links, releases, crossings, starts) is one queue, handled in the order it happened. Each job carries
/// the links as they were when it was queued, so a link that reconnects meanwhile is not touched by a job
/// meant for the old one.
enum Job {
    Msg(DragLink, Msg),
    /// A system drag ended; reported from the OS's thread. Weak: a pump that is gone has nothing left to do.
    NativeEnded { pump: std::sync::Weak<std::sync::Mutex<DragPump>>, send: SendFn, drag: u32, took: bool },
    Announced { link: DragLink, drag: u32 },
    Up(Vec<DragLink>),
    Away(Vec<DragLink>),
    Start { link: DragLink, drag: u32, files: Vec<PathBuf> },
    Flush(Sender<()>),
}

/// Where a link's router hands the drag messages it receives: into the hub's queue, never into a lock.
#[derive(Clone)]
pub struct DragInbox {
    link: DragLink,
    jobs: Sender<Job>,
}

impl DragInbox {
    pub fn deliver(&self, msg: Msg) {
        queue(&self.jobs, Job::Msg(self.link.clone(), msg));
    }
}

/// Every live link's `DragPump` and this machine's drag source, behind the forwarder's drag hooks.
///
/// The hooks run under the forwarder's lock, on the event tap's thread on a Mac, where blocking stalls every
/// key and mouse event and a slow tap gets disabled by the OS. A hook does only what must happen at that
/// moment and nothing that can wait on a disk: read the drag pasteboard (the generation, and at a crossing
/// the file list, so the decision to cancel the local drag is about files and the files are the ones dragged
/// then), put `DragSoon` on the link, and queue the rest. The pumps belong to the hub's thread alone.
pub struct DragHub {
    source: Option<Arc<dyn DragSource>>,
    /// Drag generation when the primary button went down; a drag started during this press if it moved.
    baseline: Option<u64>,
    links: Links,
    jobs: Sender<Job>,
}

impl DragHub {
    pub fn busy(&self, peer: &str) -> bool { self.links.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|l| l.peer == peer).is_some_and(|l| lock(&l.pump).busy()) }
    /// `source: None` on a machine that cannot start drags yet: it still receives them.
    pub fn new(source: Option<Box<dyn DragSource>>) -> Self {
        let source: Option<Arc<dyn DragSource>> = source.map(Arc::from);
        let (jobs, rx) = channel();
        std::thread::Builder::new().name("flowseam-drags".into()).spawn(move || run_jobs(rx)).expect("spawn drag thread");
        DragHub { source, baseline: None, links: Arc::default(), jobs }
    }

    pub fn attach(&mut self, peer: &str, pump: Arc<std::sync::Mutex<DragPump>>, send: SendFn) {
        let (weak, jobs, report_send) = (Arc::downgrade(&pump), self.jobs.clone(), send.clone());
        lock(&pump).report = Some(Arc::new(move |drag, took| {
            queue(&jobs, Job::NativeEnded { pump: weak.clone(), send: report_send.clone(), drag, took });
        }));
        let mut l = self.links.lock().unwrap_or_else(|e| e.into_inner());
        l.retain(|x| x.peer != peer);
        l.push(DragLink { peer: peer.to_string(), pump, send, next_drag: Arc::new(std::sync::atomic::AtomicU32::new(1)) });
    }

    /// The queue entry for `peer`'s router, once attached.
    pub fn inbox(&self, peer: &str) -> Option<DragInbox> {
        let l = self.links.lock().unwrap_or_else(|e| e.into_inner());
        l.iter().find(|x| x.peer == peer).map(|link| DragInbox { link: link.clone(), jobs: self.jobs.clone() })
    }

    /// Forget `peer`'s pump if it is still this one (a newer session may have replaced it).
    pub fn detach(&mut self, peer: &str, pump: &Arc<std::sync::Mutex<DragPump>>) {
        self.links.lock().unwrap_or_else(|e| e.into_inner()).retain(|x| !(x.peer == peer && Arc::ptr_eq(&x.pump, pump)));
    }

    fn snapshot(&self) -> Vec<DragLink> {
        self.links.lock().unwrap_or_else(|e| e.into_inner()).clone()
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
            queue(&self.jobs, Job::Up(self.snapshot()));
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

    /// The cursor leaves for `peer`. A drag received here and not dropped goes back. A drag of files that
    /// started here during this press is announced to `peer` now and streamed from the hub's thread. True
    /// only when files left, so the caller cancels the local drag; a text or image drag is left alone.
    pub fn drag_away(&mut self, peer: &str) -> bool {
        let all = self.snapshot();
        queue(&self.jobs, Job::Away(all.clone()));
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
        if !started {
            return false;
        }
        let files = match self.source.as_ref().map(|s| s.files()) {
            Some(Ok(f)) if !f.is_empty() => f,
            Some(Ok(_)) | None => return false, // text or an image was dragged, not files
            Some(Err(e)) => {
                eprintln!("flowseam files: dragged files unreadable: {e}");
                return false;
            }
        };
        let Some(link) = all.into_iter().find(|x| x.peer == peer) else {
            eprintln!("flowseam files: no live link to {peer}; drag not sent");
            return false;
        };
        let drag = match link.next_drag.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => link.next_drag.fetch_add(1, std::sync::atomic::Ordering::SeqCst), // 0 is never an id
            d => d,
        };
        // Queued before `DragSoon` leaves, so the peer's answer to it can only be handled after.
        queue(&self.jobs, Job::Announced { link: link.clone(), drag });
        // On the link before any input that follows the crossing (the release above all).
        (link.send)(Msg::DragSoon { drag });
        queue(&self.jobs, Job::Start { link, drag, files });
        true
    }

    /// Wait until every hook so far has been handled (tests, and anyone who needs the files settled).
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = channel();
        self.jobs.send(Job::Flush(tx)).is_ok() && rx.recv_timeout(timeout).is_ok()
    }
}

/// The drag thread only goes away by panicking (it holds the receiver for good); after that every drag is
/// lost, so each lost one says so.
fn queue(jobs: &Sender<Job>, job: Job) {
    if jobs.send(job).is_err() {
        eprintln!("flowseam files: the drag thread is gone; file drags are off until restart");
    }
}

fn run_jobs(rx: Receiver<Job>) {
    while let Ok(job) = rx.recv() {
        match job {
            Job::Msg(link, msg) => {
                for p in lock(&link.pump).on_msg(msg, &*link.send) {
                    eprintln!("flowseam files: saved {}", p.display());
                }
            }
            Job::NativeEnded { pump, send, drag, took } => {
                if let Some(pump) = pump.upgrade() {
                    for p in lock(&pump).on_native_end(drag, took, &*send) {
                        eprintln!("flowseam files: saved {}", p.display());
                    }
                }
            }
            Job::Up(all) => {
                for link in &all {
                    for p in lock(&link.pump).on_primary_up(&*link.send) {
                        eprintln!("flowseam files: saved {}", p.display());
                    }
                }
            }
            Job::Away(all) => {
                for link in &all {
                    lock(&link.pump).on_cross_away(&*link.send);
                }
            }
            Job::Announced { link, drag } => lock(&link.pump).announced(drag, &*link.send),
            Job::Start { link, drag, files } => lock(&link.pump).start_announced(drag, &files, link.send.clone()),
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

fn lock(p: &std::sync::Mutex<DragPump>) -> std::sync::MutexGuard<'_, DragPump> {
    p.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn busy_tracks_every_transfer_state() {
        struct Stub;
        impl NativeSession for Stub { fn cancel(&self) {} }
        let t = Tmp::new("busy");
        let mut p = DragPump::new(t.sub("staging"), t.sub("dest"));
        assert!(!p.busy());
        p.pending = Some((1, false)); assert!(p.busy()); p.pending = None;
        p.outgoing = Some((1, None)); assert!(p.busy()); p.outgoing = None;
        p.session = Some((1, Box::new(Stub))); assert!(p.busy()); p.session = None;
        p.incoming = Some(Incoming::begin(&p.staging, &p.dest, 1, &[]).unwrap()); assert!(p.busy());
    }

    #[test]
    fn hub_busy_is_scoped_to_peer() {
        let mut h = DragHub::new(None);
        let p = Arc::new(Mutex::new(DragPump::new(PathBuf::new(), PathBuf::new())));
        h.attach("peer", p.clone(), Arc::new(|_| true));
        assert!(!h.busy("peer")); assert!(!h.busy("missing"));
        p.lock().unwrap().pending = Some((1, false)); assert!(h.busy("peer")); assert!(!h.busy("missing"));
    }

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
        pump.lock().unwrap().on_msg(Msg::DragSoon { drag: 3 }, &noop);
        pump.lock().unwrap().on_msg(Msg::DragBegin { drag: 3, files: vec![FileMeta { name: "got.txt".into(), size: 2 }] }, &noop);
        pump.lock().unwrap().on_msg(Msg::FileChunk { drag: 3, file: 0, data: b"hi".to_vec() }, &noop);
        hub.primary(false);
        assert!(hub.flush(Duration::from_secs(2)));
        assert_eq!(fs::read(t.0.join("dest/got.txt")).unwrap(), b"hi");
        // A received drag carried back out instead is cancelled toward its sender.
        pump.lock().unwrap().on_msg(Msg::DragSoon { drag: 4 }, &noop);
        pump.lock().unwrap().on_msg(Msg::DragBegin { drag: 4, files: vec![FileMeta { name: "no.txt".into(), size: 2 }] }, &noop);
        hub.drag_away("peer");
        assert!(hub.flush(Duration::from_secs(2)));
        assert!(sent.lock().unwrap().contains(&Msg::DragCancel { drag: 4 }));
        assert!(entries(&t.0.join("staging")).is_empty());
    }

    /// The drag is on the link before the crossing returns, ahead of any input that follows it; the
    /// files streamed are the ones dragged at the crossing, even if the pasteboard changes right after.
    #[test]
    fn a_crossing_announces_at_once_and_sends_the_files_dragged_then() {
        let t = Tmp::new("announce");
        let src = t.sub("src");
        fs::write(src.join("then.txt"), b"then").unwrap();
        fs::write(src.join("later.txt"), b"later").unwrap();
        let (mut hub, state, sent, _) = hub_with(&t, &src.join("then.txt"));
        hub.primary(true);
        state.lock().unwrap().0 += 1;
        assert!(hub.drag_away("peer"));
        assert!(matches!(sent.lock().unwrap().first(), Some(Msg::DragSoon { .. })), "{:?}", sent.lock().unwrap());
        state.lock().unwrap().1 = vec![src.join("later.txt")];
        assert!(hub.flush(Duration::from_secs(2)));
        let names: Vec<String> = sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| match m {
                Msg::DragBegin { files, .. } => Some(files[0].name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["then.txt"]);
    }

    /// Text or an image dragged to the edge is not a file drag: nothing is announced and the local drag is
    /// left alone (the caller would otherwise cancel it with Escape).
    #[test]
    fn a_drag_without_files_is_left_alone() {
        let t = Tmp::new("text");
        let (mut hub, state, sent, _) = hub_with(&t, &t.0.join("unused"));
        state.lock().unwrap().1.clear();
        hub.primary(true);
        state.lock().unwrap().0 += 1;
        assert!(!hub.drag_away("peer"));
        assert!(hub.flush(Duration::from_secs(2)));
        assert!(sent.lock().unwrap().is_empty(), "{:?}", sent.lock().unwrap());
    }

    /// Let go before `DragBegin` arrives (it trails `DragSoon` by a stat): the drop still happens.
    #[test]
    fn a_release_before_the_data_is_announced_is_still_the_drop() {
        let t = Tmp::new("early-up");
        let (staging, dest) = (t.sub("staging"), t.sub("dest"));
        let noop = |_m: Msg| true;
        let mut pump = DragPump::new(staging, dest.clone());
        pump.on_msg(Msg::DragSoon { drag: 5 }, &noop);
        assert!(pump.on_primary_up(&noop).is_empty());
        pump.on_msg(Msg::DragBegin { drag: 5, files: vec![FileMeta { name: "a.txt".into(), size: 2 }] }, &noop);
        let landed = pump.on_msg(Msg::FileChunk { drag: 5, file: 0, data: b"ok".to_vec() }, &noop);
        assert_eq!(landed, vec![dest.join("a.txt")]);
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"ok");
    }

    /// Carried back before `DragBegin` arrives: cancelled toward the sender, and the late data is refused.
    #[test]
    fn an_announced_drag_carried_back_is_cancelled_and_its_data_refused() {
        let t = Tmp::new("early-away");
        let (staging, dest) = (t.sub("staging"), t.sub("dest"));
        let sent = Mutex::new(Vec::new());
        let send = |m: Msg| { sent.lock().unwrap().push(m); true };
        let mut pump = DragPump::new(staging.clone(), dest.clone());
        pump.on_msg(Msg::DragSoon { drag: 6 }, &send);
        pump.on_cross_away(&send);
        assert_eq!(*sent.lock().unwrap(), vec![Msg::DragCancel { drag: 6 }]);
        pump.on_msg(Msg::DragBegin { drag: 6, files: vec![FileMeta { name: "a.txt".into(), size: 2 }] }, &send);
        pump.on_msg(Msg::FileChunk { drag: 6, file: 0, data: b"no".to_vec() }, &send);
        pump.on_primary_up(&send);
        assert!(entries(&staging).is_empty() && entries(&dest).is_empty());
    }

    /// A peer's cancel that arrives before our hub thread starts the announced drag stops it.
    #[test]
    fn a_cancel_before_the_stream_starts_stops_it() {
        let t = Tmp::new("pre-cancel");
        let src = t.sub("src");
        fs::write(src.join("a"), b"x").unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let s = sent.clone();
        let send: SendFn = Arc::new(move |m| { s.lock().unwrap().push(m); true });
        let mut pump = DragPump::new(t.sub("staging"), t.sub("dest"));
        pump.announced(7, &*send);
        pump.on_msg(Msg::DragCancel { drag: 7 }, &*send);
        pump.start_announced(7, &[src.join("a")], send.clone());
        assert!(!sent.lock().unwrap().iter().any(|m| matches!(m, Msg::DragBegin { .. })), "{:?}", sent.lock().unwrap());
    }

    /// The same through the hub, with the peer answering `DragSoon` the instant it leaves: the answer is
    /// handled after the hub learns of the drag, so it still stops the stream.
    #[test]
    fn a_cancel_answering_the_announcement_stops_the_stream() {
        let t = Tmp::new("instant-cancel");
        let src = t.sub("src");
        fs::write(src.join("a"), b"x").unwrap();
        let state = Arc::new(Mutex::new((5, vec![src.join("a")])));
        let mut hub = DragHub::new(Some(Box::new(FakeSource(state.clone()))));
        let inbox: Arc<std::sync::OnceLock<DragInbox>> = Arc::default();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (s, ib) = (sent.clone(), inbox.clone());
        hub.attach(
            "peer",
            Arc::new(Mutex::new(DragPump::new(t.sub("staging"), t.sub("dest")))),
            Arc::new(move |m| {
                if let Msg::DragSoon { drag } = m {
                    ib.get().unwrap().deliver(Msg::DragCancel { drag });
                }
                s.lock().unwrap().push(m);
                true
            }),
        );
        inbox.set(hub.inbox("peer").unwrap()).ok().unwrap();
        hub.primary(true);
        state.lock().unwrap().0 += 1;
        assert!(hub.drag_away("peer"));
        assert!(hub.flush(Duration::from_secs(2)));
        assert!(!began(&sent), "{:?}", sent.lock().unwrap());
    }

    /// The crossing, the release and an arriving chunk run on the event tap's thread and the routers. None of
    /// them may wait on a pump: a pump can be stuck writing to a slow disk, and a stuck tap freezes the
    /// mouse and keyboard of the whole machine.
    #[test]
    fn nothing_on_the_input_path_waits_on_a_busy_pump() {
        let t = Tmp::new("busy");
        let src = t.sub("src");
        fs::write(src.join("a"), b"x").unwrap();
        let (mut hub, state, sent, pump) = hub_with(&t, &src.join("a"));
        let inbox = hub.inbox("peer").unwrap();
        let busy = pump.lock().unwrap(); // a FileChunk write stuck on the disk
        let (done, finished) = channel();
        let worker = std::thread::spawn(move || {
            hub.primary(true);
            state.lock().unwrap().0 += 1;
            let left = hub.drag_away("peer");
            hub.primary(false);
            inbox.deliver(Msg::DragSoon { drag: 9 });
            done.send(left).unwrap();
            hub
        });
        let left = finished.recv_timeout(Duration::from_secs(1));
        drop(busy);
        let hub = worker.join().unwrap();
        assert_eq!(left, Ok(true), "the input path waited on the pump");
        assert!(matches!(sent.lock().unwrap().first(), Some(Msg::DragSoon { .. })), "announced at the crossing");
        assert!(hub.flush(Duration::from_secs(2)));
        assert!(began(&sent), "streamed once the pump was free: {:?}", sent.lock().unwrap());
    }

    #[test]
    fn a_drag_bigger_than_the_free_space_is_refused_before_anything_is_written() {
        let t = Tmp::new("full");
        let (staging, dest) = (t.sub("staging"), t.sub("dest"));
        let sent = Mutex::new(Vec::new());
        let send = |m: Msg| { sent.lock().unwrap().push(m); true };
        let begin = |size| Msg::DragBegin { drag: 2, files: vec![FileMeta { name: "big".into(), size }] };
        let mut pump = DragPump::new(staging.clone(), dest.clone()).with_free_space(Arc::new(|_| Ok(RESERVE_BYTES + 100)));
        pump.on_msg(Msg::DragSoon { drag: 2 }, &send);
        pump.on_msg(begin(101), &send);
        assert_eq!(*sent.lock().unwrap(), vec![Msg::DragCancel { drag: 2 }]);
        assert!(entries(&staging).is_empty());
        sent.lock().unwrap().clear();
        pump.on_msg(Msg::DragSoon { drag: 2 }, &send);
        pump.on_msg(begin(100), &send);
        assert!(sent.lock().unwrap().is_empty(), "exactly fits: accepted");
        // Space that cannot be measured is not a refusal.
        let mut pump = DragPump::new(staging, dest).with_free_space(Arc::new(|_| Err("statvfs failed".into())));
        pump.on_msg(Msg::DragSoon { drag: 2 }, &send);
        pump.on_msg(begin(u64::MAX), &send);
        assert!(sent.lock().unwrap().is_empty());
    }

    /// A system drag that hands its `ended` to the test and remembers whether it was cancelled.
    type Ended = Box<dyn FnOnce(bool) + Send>;
    type Began = (u32, Arc<Staged>, Option<Ended>, Arc<std::sync::atomic::AtomicBool>);
    #[derive(Clone, Default)]
    struct FakeNative(Arc<Mutex<Vec<Began>>>);
    struct FakeSession(Arc<std::sync::atomic::AtomicBool>);
    impl NativeSession for FakeSession {
        fn cancel(&self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    impl NativeDrop for FakeNative {
        fn begin(&self, drag: u32, staged: Arc<Staged>, ended: Ended) -> Result<Box<dyn NativeSession>, String> {
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.0.lock().unwrap().push((drag, staged, Some(ended), cancelled.clone()));
            Ok(Box::new(FakeSession(cancelled)))
        }
    }
    impl FakeNative {
        fn staged(&self) -> Arc<Staged> {
            self.0.lock().unwrap().last().expect("a system drag began").1.clone()
        }
        fn end(&self, took: bool) {
            let ended = self.0.lock().unwrap().last_mut().expect("a system drag began").2.take().expect("ended once");
            ended(took);
        }
        fn cancelled(&self) -> bool {
            self.0.lock().unwrap().last().expect("a system drag began").3.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// A pump receiving `drag` of one 10-byte file into a system drag; the report comes back on `ends`.
    fn native_pump(t: &Tmp) -> (DragPump, FakeNative, Receiver<(u32, bool)>) {
        let fake = FakeNative::default();
        let mut pump = DragPump::new(t.sub("staging"), t.sub("dest")).with_native(Arc::new(fake.clone()));
        let (tx, ends) = channel();
        let tx = Mutex::new(tx);
        pump.report = Some(Arc::new(move |d, took| {
            let _ = tx.lock().unwrap().send((d, took));
        }));
        let noop = |_m: Msg| true;
        pump.on_msg(Msg::DragSoon { drag: 1 }, &noop);
        pump.on_msg(Msg::DragBegin { drag: 1, files: vec![FileMeta { name: "a.txt".into(), size: 10 }] }, &noop);
        (pump, fake, ends)
    }

    /// The target takes the file while it is still arriving: it waits for the rest, gets all of it where it asked,
    /// and nothing lands in Downloads.
    #[test]
    fn native_target_takes_a_file_still_arriving() {
        let t = Tmp::new("native-take");
        let (mut pump, fake, ends) = native_pump(&t);
        let noop = |_m: Msg| true;
        pump.on_msg(Msg::FileChunk { drag: 1, file: 0, data: b"01234".to_vec() }, &noop);
        let staged = fake.staged();
        let to = t.sub("target").join("a.txt");
        let to2 = to.clone();
        let (took_tx, taken) = channel();
        std::thread::spawn(move || took_tx.send(staged.deliver(0, &to2)));
        assert!(taken.recv_timeout(Duration::from_millis(100)).is_err(), "the target waits for the rest of the file");
        fake.end(true);
        let (d, took) = ends.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(pump.on_native_end(d, took, &noop).is_empty());
        pump.on_msg(Msg::FileChunk { drag: 1, file: 0, data: b"56789".to_vec() }, &noop);
        assert_eq!(taken.recv_timeout(Duration::from_secs(2)), Ok(Ok(())));
        assert_eq!(fs::read(&to).unwrap(), b"0123456789");
        assert!(entries(&t.0.join("dest")).is_empty(), "nothing in Downloads");
        assert!(pump.incoming.is_none(), "the drag is finished");
    }

    /// Ended over nothing that takes files: the files land in Downloads, as a plain release would have done.
    #[test]
    fn native_drag_over_nothing_saves_to_downloads() {
        let t = Tmp::new("native-none");
        let (mut pump, fake, ends) = native_pump(&t);
        let noop = |_m: Msg| true;
        pump.on_msg(Msg::FileChunk { drag: 1, file: 0, data: b"0123456789".to_vec() }, &noop);
        fake.end(false);
        let (d, took) = ends.recv_timeout(Duration::from_secs(2)).unwrap();
        let landed = pump.on_native_end(d, took, &noop);
        assert_eq!(landed, vec![t.0.join("dest/a.txt")]);
        assert_eq!(fs::read(t.0.join("dest/a.txt")).unwrap(), b"0123456789");
    }

    /// The release on this machine is the system drag's own drop: it must not also save to Downloads.
    #[test]
    fn native_drag_owns_the_release() {
        let t = Tmp::new("native-up");
        let (mut pump, _fake, _ends) = native_pump(&t);
        let noop = |_m: Msg| true;
        pump.on_msg(Msg::FileChunk { drag: 1, file: 0, data: b"0123456789".to_vec() }, &noop);
        assert!(pump.on_primary_up(&noop).is_empty());
        assert!(entries(&t.0.join("dest")).is_empty());
    }

    /// Carried back over the edge: the system drag is cancelled, the sender is told, and a target still waiting
    /// for the file is let go with an error instead of waiting forever.
    #[test]
    fn native_drag_carried_back_is_cancelled_and_frees_the_target() {
        let t = Tmp::new("native-back");
        let (mut pump, fake, _ends) = native_pump(&t);
        let sent = Mutex::new(Vec::new());
        let send = |m: Msg| { sent.lock().unwrap().push(m); true };
        pump.on_msg(Msg::FileChunk { drag: 1, file: 0, data: b"01234".to_vec() }, &send);
        let staged = fake.staged();
        let to = t.sub("target").join("a.txt");
        let (took_tx, taken) = channel();
        std::thread::spawn(move || took_tx.send(staged.deliver(0, &to)));
        std::thread::sleep(Duration::from_millis(50));
        pump.on_cross_away(&send);
        assert!(fake.cancelled());
        assert!(sent.lock().unwrap().contains(&Msg::DragCancel { drag: 1 }));
        assert!(taken.recv_timeout(Duration::from_secs(2)).expect("the waiting target is let go").is_err());
        // A late report of the cancelled drag changes nothing.
        assert!(pump.on_native_end(1, true, &send).is_empty());
    }

    /// A reader of a virtual file (Windows) gets the bytes as they arrive, and all of them.
    #[test]
    fn native_reader_follows_the_file_as_it_grows() {
        let t = Tmp::new("native-read");
        let (mut pump, fake, _ends) = native_pump(&t);
        let noop = |_m: Msg| true;
        let staged = fake.staged();
        pump.on_msg(Msg::FileChunk { drag: 1, file: 0, data: b"01234".to_vec() }, &noop);
        let (read_tx, read) = channel();
        std::thread::spawn(move || {
            let _ = read_tx.send(staged.wait_past(0, 0));
            let _ = read_tx.send(staged.wait_past(0, 5));
        });
        assert_eq!(read.recv_timeout(Duration::from_secs(2)), Ok(Ok(5)), "what has arrived is readable at once");
        assert!(read.recv_timeout(Duration::from_millis(50)).is_err(), "waits for bytes not yet here");
        pump.on_msg(Msg::FileChunk { drag: 1, file: 0, data: b"56789".to_vec() }, &noop);
        assert_eq!(read.recv_timeout(Duration::from_secs(2)), Ok(Ok(10)));
    }

    /// Through the hub: the end is reported from the OS's thread and handled on the hub's, in order.
    #[test]
    fn native_end_from_another_thread_is_handled_by_the_hub() {
        let t = Tmp::new("native-hub");
        let fake = FakeNative::default();
        let mut hub = DragHub::new(None);
        let pump = Arc::new(Mutex::new(DragPump::new(t.sub("staging"), t.sub("dest")).with_native(Arc::new(fake.clone()))));
        hub.attach("peer", pump, Arc::new(|_| true));
        let inbox = hub.inbox("peer").unwrap();
        inbox.deliver(Msg::DragSoon { drag: 4 });
        inbox.deliver(Msg::DragBegin { drag: 4, files: vec![FileMeta { name: "b.txt".into(), size: 3 }] });
        inbox.deliver(Msg::FileChunk { drag: 4, file: 0, data: b"abc".to_vec() });
        assert!(hub.flush(Duration::from_secs(2)));
        let f = fake.clone();
        std::thread::spawn(move || f.end(false)).join().unwrap();
        assert!(hub.flush(Duration::from_secs(2)));
        assert_eq!(fs::read(t.0.join("dest/b.txt")).unwrap(), b"abc");
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
        a.lock().unwrap().start(1, &[src.join("movie.mov")], send_ab.clone());
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
        a.lock().unwrap().start(2, &[src.join("second.txt")], send_ab.clone());
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
