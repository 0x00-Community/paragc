//! 目录里的公开堆登记：堆号领取（`.paragc-<堆号>` 标记文件）与死活判断。
//!
//! 公开堆跟着主人（建堆的进程）死：主人退出或被杀、又没有 persist，堆就死了。判断死活只读堆文件超级块开头的几个字段，
//! 不用映射整个堆，所以回收方可以替一个自己没打开过的堆判死活；判定死了就把堆文件与标记删掉（文件内存等最后一个映射它的进程放手后还给系统）。
//!
//! 领号、判过期、删标记都在标记文件的 `flock` 里做（锁到之后核对路径还是这个文件，没被别人删掉重建）：
//! 「看一眼再删」这种两步不会和别人的领号交错。进程被杀时内核替它放锁。
use core::mem::offset_of;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::layout::{MAGIC, Super};
use super::process::Process;
use super::region::{WINDOW, WINDOWS};

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
