//! [`PublicSpace`]：本进程的公开地址区；在它上面建、打开公开堆。
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicU64, Ordering};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;

use super::heap::{Backing, Inner, PublicHeap};
use super::layout::MAGIC;
use super::process::Process;
use super::region::{Region, WINDOW, WINDOWS};

const _: () = assert!(WINDOWS <= u64::BITS as usize, "窗口占用位图是一个 u64");

/// 超级块开头三个字（魔数、窗口大小、堆号）的字节数：打开时先读它们，才知道映射到哪个窗口
const PROLOGUE: usize = 3 * size_of::<u64>();
/// 堆号标记文件名的前缀（放在堆文件所在目录：`.paragc-<堆号>`，内容是「建堆进程号 启动时间」一行 + 堆文件名）
const MARKER_PREFIX: &str = ".paragc-";


pub(crate) struct Space {
    region: Region,
    /// 窗口占用位图
    used: AtomicU64,
}

impl Space {
    pub(crate) fn region(&self) -> &Region {
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

    pub(crate) fn release(&self, k: usize) {
        self.used.fetch_and(!(1 << k), Ordering::AcqRel);
    }
}

/// 本进程的公开地址区：保留一块地址（不占内存），按堆号切成窗口；公开堆 k 映射在第 k 个窗口，
/// 所以公开堆里存的偏移在每个进程里都指向同一个对象。一个进程通常只要一个。
#[derive(Clone)]
pub struct PublicSpace(Arc<Space>);

impl PublicSpace {
    pub fn reserve() -> io::Result<Self> {
        Ok(Self(Arc::new(Space { region: Region::reserve()?, used: AtomicU64::new(0) })))
    }

    /// 在 `path`（共享内存文件系统上的新文件，比如 `/dev/shm/…` 或自己挂的 tmpfs / hugetlbfs）建一个公开堆。
    /// 文件刚建时是空的，用到哪里延长到哪里；权限 0600，有文件权限的进程才能打开。
    ///
    /// 堆号在同一个目录里跨进程唯一：用 `O_EXCL` 建 `.paragc-<堆号>` 标记文件领号（标记里的堆文件已经不在、或堆号对不上时收回重用）。
    /// 这个句柄（连同它建出的 [`crate::Public`]）全部析构时删除堆文件与标记，除非调过 [`PublicHeap::persist`]。
    pub fn create(&self, path: impl AsRef<Path>, threads: NonZeroUsize) -> io::Result<PublicHeap> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = path.as_ref();
        let (k, marker) = self.claim_number(path)?;
        let file = match OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(path) {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_file(&marker);
                self.0.release(k);
                return Err(e);
            }
        };
        let backing = Backing::File { file, path: path.to_path_buf(), marker: Some(marker.clone()) };
        self.attach(k, backing, threads, true).inspect_err(|_| {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(&marker);
        })
    }

    /// 领一个堆号：本地址区的窗口空着，并且在堆文件所在目录建得出 `.paragc-<堆号>`。返回堆号与标记文件。
    fn claim_number(&self, path: &Path) -> io::Result<(usize, std::path::PathBuf)> {
        use std::io::Write;
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "堆文件路径没有文件名"))?;
        for k in 0..WINDOWS {
            if !self.0.claim(k) {
                continue;
            }
            let marker = dir.join(format!("{MARKER_PREFIX}{k}"));
            if Self::stale(&marker, dir, k) {
                let _ = std::fs::remove_file(&marker);
            }
            match OpenOptions::new().write(true).create_new(true).open(&marker) {
                Ok(mut f) => {
                    let me = Process::current();
                    f.write_all(format!("{} {}\n", me.pid, me.start).as_bytes())?;
                    f.write_all(name.as_encoded_bytes())?;
                    return Ok((k, marker));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => self.0.release(k),
                Err(e) => {
                    self.0.release(k);
                    return Err(e);
                }
            }
        }
        Err(io::Error::new(io::ErrorKind::OutOfMemory, "公开堆号用完了（这个目录里的标记或本地址区的窗口）"))
    }

    /// 标记文件过期了：它写的堆文件在、但堆号不是 `k`；或堆文件不在、建堆的进程也死了（进程还活着就当它还在建堆，不抢）。
    fn stale(marker: &Path, dir: &Path, k: usize) -> bool {
        let Ok(text) = std::fs::read(marker) else { return false };
        let Some(split) = text.iter().position(|&b| b == b'\n') else { return false };
        let (owner, name) = (&text[..split], &text[split + 1..]);
        if let Ok(f) = File::open(dir.join(std::ffi::OsStr::from_bytes(name))) {
            return Self::heap_number(&f).map_or(true, |n| n != k);
        }
        let mut words = std::str::from_utf8(owner).unwrap_or_default().split_whitespace().map(str::parse::<u64>);
        match (words.next(), words.next()) {
            (Some(Ok(pid)), Some(Ok(start))) => u32::try_from(pid).is_ok_and(|pid| !Process { pid, start }.alive()),
            _ => true,
        }
    }

    /// 没有文件的公开堆（只在本进程里用；测试与 Miri 用它）。
    pub fn create_anonymous(&self, threads: NonZeroUsize) -> io::Result<PublicHeap> {
        let k = self.0.claim_any().ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "公开地址区的窗口用完了"))?;
        self.attach(k, Backing::Anonymous, threads, true)
    }

    /// 打开别人（或本进程另一个地址区）建的公开堆，映射到它的堆号对应的窗口。
    pub fn open(&self, path: impl AsRef<Path>, threads: NonZeroUsize) -> io::Result<PublicHeap> {
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let k = Self::heap_number(&file)?;
        if !self.0.claim(k) {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "这个公开堆的窗口在本地址区已被占用"));
        }
        self.attach(k, Backing::File { file, path: path.to_path_buf(), marker: None }, threads, false)
    }

    /// 读超级块开头，核对魔数与窗口大小，返回堆号。
    fn heap_number(file: &File) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let mut head = [0; PROLOGUE];
        file.read_exact_at(&mut head, 0)?;
        let word = |i: usize| u64::from_le_bytes(head[i * size_of::<u64>()..(i + 1) * size_of::<u64>()].try_into().expect("8 字节"));
        if word(0) != MAGIC || word(1) != WINDOW as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "不是 paragc 公开堆，或布局版本不同"));
        }
        usize::try_from(word(2)).ok().filter(|&k| k < WINDOWS).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "堆号超出窗口数"))
    }

    /// 把窗口 `k` 映射上 `backing`，`fresh` 时写新的超级块。失败时还掉窗口。
    fn attach(&self, k: usize, backing: Backing, threads: NonZeroUsize, fresh: bool) -> io::Result<PublicHeap> {
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
        Inner::new(self.0.clone(), k, backing, threads, fresh).map(PublicHeap::from)
    }
}
