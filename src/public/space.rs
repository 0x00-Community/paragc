//! [`PublicSpace`]：本进程的公开地址区；在它上面建、打开公开堆。
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicU64, Ordering};
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::sync::Arc;

use super::heap::{Backing, Inner, PublicHeap};
use super::region::{Region, WINDOWS};
use super::registry::{FileHeader, Registry};

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
pub struct PublicSpace(Arc<Space>);

impl PublicSpace {
    pub fn reserve() -> io::Result<Self> {
        Ok(Self(Arc::new(Space { region: Region::reserve()?, used: AtomicU64::new(0) })))
    }

    /// 在 `path`（共享内存文件系统上的新文件，比如 `/dev/shm/…` 或自己挂的 tmpfs / hugetlbfs）建一个公开堆。
    /// 文件刚建时是空的，用到哪里延长到哪里；权限 0600，有文件权限的进程才能打开。
    ///
    /// 堆号在同一个目录里跨进程唯一：在 `.paragc-<堆号>` 标记文件的锁里领号（标记指着的堆已经死了时收回重用）。
    /// 本进程是这个堆的主人：这个句柄（连同它建出的 [`crate::Public`]）全部析构、或本进程退出 / 被杀，堆就跟着死，
    /// 除非调过 [`PublicHeap::persist`]。
    pub fn create(&self, path: impl AsRef<Path>, threads: NonZeroUsize) -> io::Result<PublicHeap> {
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
        let backing = Backing::File { file, path: path.to_path_buf(), owner: true };
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
    pub fn create_anonymous(&self, threads: NonZeroUsize) -> io::Result<PublicHeap> {
        let k = self.0.claim_any().ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "公开地址区的窗口用完了"))?;
        self.attach(k, Backing::Anonymous, threads, true)
    }

    /// 打开别人（或本进程另一个地址区）建的公开堆，映射到它的堆号对应的窗口。
    pub fn open(&self, path: impl AsRef<Path>, threads: NonZeroUsize) -> io::Result<PublicHeap> {
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let header = FileHeader::read(&file)?;
        if header.is_dead() {
            Registry::of(path).bury(path, header.heap, header.incarnation);
            return Err(io::Error::new(io::ErrorKind::NotFound, "这个公开堆已经死了（主人不在了）"));
        }
        let k = header.heap;
        if !self.0.claim(k) {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "这个公开堆的窗口在本地址区已被占用"));
        }
        self.attach(k, Backing::File { file, path: path.to_path_buf(), owner: false }, threads, false)
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
