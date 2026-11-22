//! 共享内存与进程（只在 Linux）：公开地址区与窗口映射 [`Region`]、[`CrossHeapSpace`]、目录里的堆号登记 [`Registry`]、进程死活 [`Process`]。
//!
//! 公开地址区：每个进程保留一块同样大小、按自身大小对齐的地址，按堆号切成固定大小的窗口；公开堆 k 永远映射在第 k 个窗口。
//!
//! 保留只占地址（`PROT_NONE` + `MAP_NORESERVE`），不占内存。Miri 跑不了文件映射，换成按同样对齐分配的一块零内存，
//! 公开堆的逻辑照样能测（只是没有文件，不能跨进程）。
//!
//! 跨进程要用的两样：认进程（进程号 + 启动时间，防进程号被复用后认错人）、共享内存上的 futex 等待 / 唤醒。
//!
//! 目录里的公开堆登记：堆号领取（`.paragc-<堆号>` 标记文件）与死活判断。
//!
//! 公开堆跟着主人（建堆的进程）死：主人退出或被杀、又没有 persist，堆就死了。判断死活只读堆文件超级块开头的几个字段，
//! 不用映射整个堆，所以回收方可以替一个自己没打开过的堆判死活；判定死了就把堆文件与标记删掉（文件内存等最后一个映射它的进程放手后还给系统）。
//!
//! 领号、判过期、删标记都在标记文件的 `flock` 里做（锁到之后核对路径还是这个文件，没被别人删掉重建）：
//! 「看一眼再删」这种两步不会和别人的领号交错。进程被杀时内核替它放锁。
//!
//! [`CrossHeapSpace`]：本进程的公开地址区；在它上面建、打开公开堆。
#![cfg(target_os = "linux")]
use core::mem::offset_of;
use core::num::NonZeroUsize;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::heap::{Backing, Inner, CrossHeap};
use crate::layout::{MAGIC, Super};

/// 一个窗口的大小（一个公开堆的全部地址）
pub const WINDOW: usize = if cfg!(miri) { 16 << 20 } else { 16 << 30 };
/// 窗口数（公开堆号的上限）
pub const WINDOWS: usize = if cfg!(miri) { 4 } else { 64 };
/// 整个公开地址区；按它对齐，所以从区内任意地址抹低位就是区的起点
pub const REGION: usize = WINDOW * WINDOWS;

const _: () = assert!(REGION.is_power_of_two() && WINDOW.is_power_of_two());

pub struct Region {
    base: NonNull<u8>,
}

// SAFETY: 只是一段地址；里面的内容由各个公开堆自己同步
unsafe impl Send for Region {}
// SAFETY: 同上
unsafe impl Sync for Region {}

impl Region {
    /// 区内地址 `p` 所在区的起点（抹掉低位）。
    pub fn base_of(p: usize) -> usize {
        p & !(REGION - 1)
    }

    pub fn base(&self) -> NonNull<u8> {
        self.base
    }

    /// 第 `k` 个窗口的起点。
    pub fn window(&self, k: usize) -> *mut u8 {
        self.base.as_ptr().wrapping_add(k * WINDOW)
    }
}

#[cfg(not(miri))]
impl Region {
    /// 保留整个区：先多保留一倍找到对齐的起点，再把两头多出来的还掉。
    pub fn reserve() -> io::Result<Self> {
        // SAFETY: 匿名、不可访问的新映射，不碰已有内存
        let raw = unsafe { libc::mmap(core::ptr::null_mut(), 2 * REGION, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0) };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let raw = raw.cast::<u8>();
        let head = raw.align_offset(REGION);
        let base = raw.wrapping_add(head);
        // SAFETY: 两段都在刚保留的映射里、不在保留下来的 [base, base + REGION) 里
        unsafe {
            if head > 0 {
                libc::munmap(raw.cast(), head);
            }
            libc::munmap(base.wrapping_add(REGION).cast(), REGION - head);
        }
        Ok(Self { base: NonNull::new(base).expect("mmap 不会返回空") })
    }

    /// 把文件映射到第 `k` 个窗口（整个窗口一次映射；文件比窗口短，只访问已经延长过的部分）。
    /// SAFETY（调用方）：窗口 `k` 没被占用。
    pub unsafe fn map_file(&self, k: usize, file: &File) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let at = self.window(k);
        // SAFETY: 见上；MAP_FIXED 只覆盖本区保留的这一段
        let p = unsafe { libc::mmap(at.cast(), WINDOW, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_FIXED, file.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// 没有文件的公开堆：窗口换成共享的匿名内存（同一进程内可用）。SAFETY 同 [`Region::map_file`]。
    pub unsafe fn map_anonymous(&self, k: usize) -> io::Result<()> {
        let at = self.window(k);
        // SAFETY: 同上
        let p = unsafe { libc::mmap(at.cast(), WINDOW, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE, -1, 0) };
        if p == libc::MAP_FAILED { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// 窗口不用了：换回不可访问的保留（地址仍归本区）。SAFETY（调用方）：之后没人再访问这个窗口。
    pub unsafe fn unmap(&self, k: usize) {
        let at = self.window(k);
        // SAFETY: 见上
        unsafe { libc::mmap(at.cast(), WINDOW, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE, -1, 0) };
    }

    /// 把文件延长到至少覆盖 `[offset, offset + len)`（只会变长；多个线程同时调也安全）。
    pub fn commit(file: &File, offset: usize, len: usize) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let (offset, len) = (i64::try_from(offset).expect("窗口内偏移"), i64::try_from(len).expect("窗口内长度"));
        // SAFETY: 只改文件长度与分配
        if unsafe { libc::fallocate(file.as_raw_fd(), 0, offset, len) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    /// 文件的 `[offset, offset + len)` 打洞（长度不变，之后读到零）。尽力而为，失败就留着。
    pub fn punch(file: &File, offset: usize, len: usize) {
        use std::os::fd::AsRawFd;
        let (Ok(offset), Ok(len)) = (i64::try_from(offset), i64::try_from(len)) else { return };
        // SAFETY: 只丢文件这一段的内容
        unsafe { libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, offset, len) };
    }

    /// 共享匿名内存的 `[at, at + len)` 还给系统（之后读到零）。尽力而为。
    pub fn remove(at: *mut u8, len: usize) {
        // SAFETY: 调用方给的是本区一个窗口里的一段，里面没有还要用的东西
        unsafe { libc::madvise(at.cast(), len, libc::MADV_REMOVE) };
    }
}

#[cfg(not(miri))]
impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: 整个区归它，窗口里的堆都已析构（它们持有区的 Arc）
        unsafe { libc::munmap(self.base.as_ptr().cast(), REGION) };
    }
}

#[cfg(miri)]
impl Region {
    const LAYOUT: std::alloc::Layout = match std::alloc::Layout::from_size_align(REGION, REGION) {
        Ok(l) => l,
        Err(_) => panic!("区的大小 / 对齐不合法"),
    };

    pub fn reserve() -> io::Result<Self> {
        // SAFETY: 大小非零
        let p = unsafe { std::alloc::alloc_zeroed(Self::LAYOUT) };
        Ok(Self { base: NonNull::new(p).unwrap_or_else(|| std::alloc::handle_alloc_error(Self::LAYOUT)) })
    }

    pub unsafe fn map_file(&self, _: usize, _: &File) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub unsafe fn map_anonymous(&self, _: usize) -> io::Result<()> {
        Ok(())
    }

    pub unsafe fn unmap(&self, k: usize) {
        // SAFETY: 窗口在区内；之后没人再访问
        unsafe { self.window(k).write_bytes(0, WINDOW) };
    }

    pub fn punch(_: &File, _: usize, _: usize) {}

    pub fn remove(_: *mut u8, _: usize) {}

    pub fn commit(_: &File, _: usize, _: usize) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(miri)]
impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: 来自 reserve 的 alloc_zeroed
        unsafe { std::alloc::dealloc(self.base.as_ptr(), Self::LAYOUT) };
    }
}

/// 一个进程：进程号 + 启动时间（开机以来的时钟滴答，`/proc/<pid>/stat` 第 22 个字段）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub start: u64,
}

/// `/proc/<pid>/stat` 里右括号之后：第 3 个字段（状态）是第 0 个，第 22 个字段（启动时间）是第 19 个
const STATE_FIELD: usize = 0;
const START_FIELD: usize = 19;

impl Process {
    /// 本进程。
    pub fn current() -> Self {
        let pid = std::process::id();
        Self { pid, start: Self::stat(pid).map_or(0, |(_, start)| start) }
    }

    /// （状态字符, 启动时间）；进程不在了返回 None。Miri 下不读 /proc。
    #[cfg(not(miri))]
    fn stat(pid: u32) -> Option<(char, u64)> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // 进程名在括号里、可能含空格，从最后一个右括号之后数
        let rest = &text[text.rfind(')')? + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        Some((fields.get(STATE_FIELD)?.chars().next()?, fields.get(START_FIELD)?.parse().ok()?))
    }

    #[cfg(miri)]
    fn stat(_: u32) -> Option<(char, u64)> {
        None
    }

    /// 还活着：/proc 里有它、不是僵尸、启动时间对得上。启动时间记成 0（还在登记，或读不到 /proc）时一律当活着。
    pub fn alive(self) -> bool {
        if self.start == 0 || cfg!(miri) {
            return true;
        }
        Self::stat(self.pid).is_some_and(|(state, start)| start == self.start && !matches!(state, 'Z' | 'X'))
    }
}

/// 标记文件名的前缀：`.paragc-<堆号>`，内容是「建堆进程号 启动时间」一行 + 堆文件名（空 = 号空着）
const MARKER_PREFIX: &str = ".paragc-";

/// 一个堆还在不在。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Liveness {
    Alive,
    /// 死了（主人不在了、没有 persist），文件与标记已经删掉
    Dead,
    /// 要找的那个堆（按身份）已经不在了：这个号现在是别的堆的，或空着
    Gone,
    /// 判断不了（读文件出错等）：当它还活着
    Unknown,
}

/// 从堆文件里读出来的超级块开头几个字段。
pub struct FileHeader {
    pub heap: usize,
    pub incarnation: u64,
    dead: bool,
    persist: bool,
    owner: Process,
}

impl FileHeader {
    /// 读堆文件的超级块；不是 paragc 公开堆（魔数、窗口大小、堆号不对）时报错。
    pub fn read(file: &File) -> io::Result<Self> {
        let mut buf = [0u8; size_of::<Super>()];
        file.read_exact_at(&mut buf, 0)?;
        let u64_at = |o: usize| u64::from_le_bytes(buf[o..o + size_of::<u64>()].try_into().expect("8 字节"));
        let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + size_of::<u32>()].try_into().expect("4 字节"));
        if u64_at(offset_of!(Super, magic)) != MAGIC || u64_at(offset_of!(Super, window)) != WINDOW as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "不是 paragc 公开堆，或布局版本不同"));
        }
        let heap = usize::try_from(u64_at(offset_of!(Super, heap))).ok().filter(|&k| k < WINDOWS).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "堆号超出窗口数"))?;
        Ok(Self {
            heap,
            incarnation: u64_at(offset_of!(Super, incarnation)),
            dead: u32_at(offset_of!(Super, dead)) != 0,
            persist: u32_at(offset_of!(Super, persist)) != 0,
            owner: Process { pid: u32_at(offset_of!(Super, owner_pid)), start: u64_at(offset_of!(Super, owner_start)) },
        })
    }

    /// 这个堆死了：标过死，或主人不在了又没有 persist。
    pub fn is_dead(&self) -> bool {
        self.dead || (!self.persist && !self.owner.alive())
    }
}

/// 标记文件的内容。
struct Marker {
    creator: Option<Process>,
    name: Vec<u8>,
}

impl Marker {
    fn parse(text: &[u8]) -> Option<Self> {
        let split = text.iter().position(|&b| b == b'\n')?;
        let mut words = std::str::from_utf8(&text[..split]).ok()?.split_whitespace().map(str::parse::<u64>);
        let creator = match (words.next(), words.next()) {
            (Some(Ok(pid)), Some(Ok(start))) => u32::try_from(pid).ok().map(|pid| Process { pid, start }),
            _ => None,
        };
        Some(Self { creator, name: text[split + 1..].to_vec() })
    }
}

/// 一个目录里的公开堆登记。
pub struct Registry<'d> {
    dir: &'d Path,
}

impl<'d> Registry<'d> {
    /// 堆文件 `path` 所在目录的登记。
    pub fn of(path: &'d Path) -> Self {
        Self { dir: path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new(".")) }
    }

    pub fn dir(&self) -> &Path {
        self.dir
    }

    pub fn marker(&self, k: usize) -> PathBuf {
        self.dir.join(format!("{MARKER_PREFIX}{k}"))
    }

    /// 打开并锁住堆号 `k` 的标记（`create`：没有就建一个空的；否则没有时返回 NotFound）；锁到之后路径必须还是这个文件，
    /// 否则（被别人删了）重来。锁随文件关闭放开。
    fn lock(&self, k: usize, create: bool) -> io::Result<File> {
        use std::os::fd::AsRawFd;
        let path = self.marker(k);
        loop {
            let f = OpenOptions::new().read(true).write(true).create(create).truncate(false).mode(0o600).open(&path)?;
            // SAFETY: fd 有效；flock 只作用于这个打开的文件
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
            match std::fs::metadata(&path) {
                Ok(m) if m.ino() == f.metadata()?.ino() && m.dev() == f.metadata()?.dev() => return Ok(f),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// 在锁里读标记内容。
    fn read_locked(f: &mut File) -> io::Result<Vec<u8>> {
        let mut text = Vec::new();
        f.rewind()?;
        f.read_to_end(&mut text)?;
        Ok(text)
    }

    /// 领堆号 `k`（记下本进程与堆文件名 `name`）；号被一个还活着（或正在建）的堆占着时返回 Ok(false)。过期的标记在锁里收回。
    pub fn claim(&self, k: usize, name: &OsStr) -> io::Result<bool> {
        let mut f = self.lock(k, true)?;
        let text = Self::read_locked(&mut f)?;
        if let Some(marker) = Marker::parse(&text)
            && self.judge(k, &marker, None) != Liveness::Dead
        {
            return Ok(false);
        }
        f.set_len(0)?;
        f.rewind()?;
        let me = Process::current();
        f.write_all(format!("{} {}\n", me.pid, me.start).as_bytes())?;
        f.write_all(name.as_encoded_bytes())?;
        Ok(true)
    }

    /// 按标记内容判堆号 `k` 上的堆（不删任何东西）。`expect`：要找的身份。
    ///
    /// 建堆是几步做完的（领号、建文件、延长文件、写超级块），别的进程随时可能读到中间状态：只要建堆进程还活着，
    /// 读到的任何不完整（文件还没有、超级块还是零或只看到一半）都当它正在建。
    fn judge(&self, k: usize, marker: &Marker, expect: Option<u64>) -> Liveness {
        let gone = if expect.is_some() { Liveness::Gone } else { Liveness::Dead };
        let creator_alive = marker.creator.is_some_and(Process::alive);
        match File::open(self.dir.join(OsStr::from_bytes(&marker.name))).and_then(|f| FileHeader::read(&f)) {
            Ok(header) if header.heap == k => {
                if expect.is_some_and(|inc| inc != header.incarnation) {
                    Liveness::Gone
                } else if header.is_dead() {
                    Liveness::Dead
                } else {
                    Liveness::Alive
                }
            }
            Ok(_) => {
                if creator_alive {
                    Liveness::Alive
                } else {
                    gone
                }
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof) => {
                if creator_alive {
                    Liveness::Alive
                } else {
                    gone
                }
            }
            Err(_) => Liveness::Unknown,
        }
    }

    /// 堆号 `k` 的堆还在不在。`expect`：要找的是哪个身份的堆（None = 这个号上的随便哪个）。判定死了时删掉它的堆文件与标记。
    pub fn liveness(&self, k: usize, expect: Option<u64>) -> Liveness {
        let text = match std::fs::read(self.marker(k)) {
            Ok(t) => t,
            // 标记没了：这个号上没有堆（死了、删掉了）
            Err(e) if e.kind() == io::ErrorKind::NotFound => return if expect.is_some() { Liveness::Gone } else { Liveness::Dead },
            Err(_) => return Liveness::Unknown,
        };
        let Some(marker) = Marker::parse(&text) else {
            // 空标记（号空着）或正在写
            return if text.is_empty() && expect.is_some() { Liveness::Gone } else { Liveness::Unknown };
        };
        let verdict = self.judge(k, &marker, expect);
        if verdict == Liveness::Dead {
            let path = self.dir.join(OsStr::from_bytes(&marker.name));
            if let Ok(header) = File::open(&path).and_then(|f| FileHeader::read(&f)) {
                self.bury(&path, k, header.incarnation);
            }
        }
        verdict
    }

    /// 删掉堆号 `k`、身份 `incarnation` 的堆的文件与标记：在标记的锁里核对标记还指着这个文件、文件还是这个身份才删
    /// （这期间号可能已经被别人重用）。
    pub fn bury(&self, path: &Path, k: usize, incarnation: u64) {
        let Ok(mut f) = self.lock(k, false) else { return };
        let Ok(text) = Self::read_locked(&mut f) else { return };
        let names_it = Marker::parse(&text).is_some_and(|m| self.dir.join(OsStr::from_bytes(&m.name)) == path);
        let same = File::open(path).and_then(|h| FileHeader::read(&h)).is_ok_and(|h| h.incarnation == incarnation);
        if names_it && same {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(self.marker(k));
        }
    }

    /// 建堆失败：还掉刚领的号（标记还是本进程为 `name` 领的才删）。
    pub fn abandon(&self, k: usize, name: &OsStr) {
        let Ok(mut f) = self.lock(k, false) else { return };
        let Ok(text) = Self::read_locked(&mut f) else { return };
        if Marker::parse(&text).is_some_and(|m| m.name == name.as_encoded_bytes() && m.creator == Some(Process::current())) {
            let _ = std::fs::remove_file(self.marker(k));
        }
    }
}

const _: () = assert!(WINDOWS <= u64::BITS as usize, "窗口占用位图是一个 u64");



pub struct Space {
    region: Region,
    /// 窗口占用位图
    used: AtomicU64,
}

impl Space {
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// 占第 `k` 个窗口；已经被占返回 false。
    fn claim(&self, k: usize) -> bool {
        self.used.fetch_or(1 << k, Ordering::AcqRel) & (1 << k) == 0
    }

    /// 占一个空窗口。
    fn claim_any(&self) -> Option<usize> {
        (0..WINDOWS).find(|&k| self.claim(k))
    }

    pub fn release(&self, k: usize) {
        self.used.fetch_and(!(1 << k), Ordering::AcqRel);
    }
}

/// 本进程的公开地址区：保留一块地址（不占内存），按堆号切成窗口；公开堆 k 映射在第 k 个窗口，
/// 所以公开堆里存的偏移在每个进程里都指向同一个对象。一个进程通常只要一个。
#[derive(Clone)]
pub struct CrossHeapSpace(Arc<Space>);

impl CrossHeapSpace {
    pub fn reserve() -> io::Result<Self> {
        Ok(Self(Arc::new(Space { region: Region::reserve()?, used: AtomicU64::new(0) })))
    }

    /// 在 `path`（共享内存文件系统上的新文件，比如 `/dev/shm/…` 或自己挂的 tmpfs / hugetlbfs）建一个公开堆。
    /// 文件刚建时是空的，用到哪里延长到哪里；权限 0600，有文件权限的进程才能打开。
    ///
    /// 堆号在同一个目录里跨进程唯一：在 `.paragc-<堆号>` 标记文件的锁里领号（标记指着的堆已经死了时收回重用）。
    /// 本进程是这个堆的主人：这个句柄（连同它建出的 [`crate::CrossHeapRoot`]）全部析构、或本进程退出 / 被杀，堆就跟着死，
    /// 除非调过 [`CrossHeap::persist`]。
    pub fn create(&self, path: impl AsRef<Path>, threads: NonZeroUsize) -> io::Result<CrossHeap> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = path.as_ref();
        let k = self.claim_number(path)?;
        let abandon = || {
            if let Some(name) = path.file_name() {
                Registry::of(path).abandon(k, name);
            }
        };
        let file = match OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(path) {
            Ok(f) => f,
            Err(e) => {
                abandon();
                self.0.release(k);
                return Err(e);
            }
        };
        let backing = Backing::File { file, path: Some(path.to_path_buf()), owner: true };
        self.attach(k, backing, threads, true).inspect_err(|_| {
            let _ = std::fs::remove_file(path);
            abandon();
        })
    }

    /// 领一个堆号：本地址区的窗口空着，并且在堆文件所在目录领得到 `.paragc-<堆号>`。
    fn claim_number(&self, path: &Path) -> io::Result<usize> {
        let registry = Registry::of(path);
        let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "堆文件路径没有文件名"))?;
        for k in 0..WINDOWS {
            if !self.0.claim(k) {
                continue;
            }
            match registry.claim(k, name) {
                Ok(true) => return Ok(k),
                Ok(false) => self.0.release(k),
                Err(e) => {
                    self.0.release(k);
                    return Err(e);
                }
            }
        }
        Err(io::Error::new(io::ErrorKind::OutOfMemory, "公开堆号用完了（这个目录里的标记或本地址区的窗口）"))
    }

    /// 没有文件的公开堆（只在本进程里用；测试与 Miri 用它）。
    pub fn create_anonymous(&self, threads: NonZeroUsize) -> io::Result<CrossHeap> {
        let k = self.0.claim_any().ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "公开地址区的窗口用完了"))?;
        self.attach(k, Backing::Anonymous, threads, true)
    }

    /// 打开别人（或本进程另一个地址区）建的公开堆，映射到它的堆号对应的窗口。
    pub fn open(&self, path: impl AsRef<Path>, threads: NonZeroUsize) -> io::Result<CrossHeap> {
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        self.open_file(file, Some(path), threads)
    }

    /// 凭文件描述符打开公开堆（别的进程经 `SCM_RIGHTS` 交来的 [`CrossHeap::fd`]），不需要能访问堆文件的路径。
    pub fn open_fd(&self, fd: std::os::fd::OwnedFd, threads: NonZeroUsize) -> io::Result<CrossHeap> {
        self.open_file(File::from(fd), None, threads)
    }

    fn open_file(&self, file: File, path: Option<&Path>, threads: NonZeroUsize) -> io::Result<CrossHeap> {
        let header = FileHeader::read(&file)?;
        if header.is_dead() {
            if let Some(path) = path {
                Registry::of(path).bury(path, header.heap, header.incarnation);
            }
            return Err(io::Error::new(io::ErrorKind::NotFound, "这个公开堆已经死了（主人不在了）"));
        }
        let k = header.heap;
        if !self.0.claim(k) {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "这个公开堆的窗口在本地址区已被占用"));
        }
        self.attach(k, Backing::File { file, path: path.map(Path::to_path_buf), owner: false }, threads, false)
    }

    /// 把窗口 `k` 映射上 `backing`，`fresh` 时写新的超级块。失败时还掉窗口。
    fn attach(&self, k: usize, backing: Backing, threads: NonZeroUsize, fresh: bool) -> io::Result<CrossHeap> {
        // SAFETY: 窗口 k 刚被占下，没人用
        let mapped = unsafe {
            match &backing {
                Backing::File { file, .. } => self.0.region.map_file(k, file),
                Backing::Anonymous => self.0.region.map_anonymous(k),
            }
        };
        if let Err(e) = mapped {
            self.0.release(k);
            return Err(e);
        }
        Inner::new(self.0.clone(), k, backing, threads, fresh).map(CrossHeap::from)
    }
}
