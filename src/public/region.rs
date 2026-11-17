//! 公开地址区：每个进程保留一块同样大小、按自身大小对齐的地址，按堆号切成固定大小的窗口；公开堆 k 永远映射在第 k 个窗口。
//!
//! 保留只占地址（`PROT_NONE` + `MAP_NORESERVE`），不占内存。Miri 跑不了文件映射，换成按同样对齐分配的一块零内存，
//! 公开堆的逻辑照样能测（只是没有文件，不能跨进程）。
use core::ptr::NonNull;
use std::fs::File;
use std::io;

/// 一个窗口的大小（一个公开堆的全部地址）
pub(crate) const WINDOW: usize = if cfg!(miri) { 16 << 20 } else { 16 << 30 };
/// 窗口数（公开堆号的上限）
pub(crate) const WINDOWS: usize = if cfg!(miri) { 4 } else { 64 };
/// 整个公开地址区；按它对齐，所以从区内任意地址抹低位就是区的起点
pub(crate) const REGION: usize = WINDOW * WINDOWS;

const _: () = assert!(REGION.is_power_of_two() && WINDOW.is_power_of_two());

pub(crate) struct Region {
    base: NonNull<u8>,
}

// SAFETY: 只是一段地址；里面的内容由各个公开堆自己同步
unsafe impl Send for Region {}
// SAFETY: 同上
unsafe impl Sync for Region {}

impl Region {
    /// 区内地址 `p` 所在区的起点（抹掉低位）。
    pub(crate) fn base_of(p: usize) -> usize {
        p & !(REGION - 1)
    }

    pub(crate) fn base(&self) -> NonNull<u8> {
        self.base
    }

    /// 第 `k` 个窗口的起点。
    pub(crate) fn window(&self, k: usize) -> *mut u8 {
        self.base.as_ptr().wrapping_add(k * WINDOW)
    }
}

#[cfg(not(miri))]
impl Region {
    /// 保留整个区：先多保留一倍找到对齐的起点，再把两头多出来的还掉。
    pub(crate) fn reserve() -> io::Result<Self> {
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
    pub(crate) unsafe fn map_file(&self, k: usize, file: &File) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let at = self.window(k);
        // SAFETY: 见上；MAP_FIXED 只覆盖本区保留的这一段
        let p = unsafe { libc::mmap(at.cast(), WINDOW, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_FIXED, file.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// 没有文件的公开堆：窗口换成共享的匿名内存（同一进程内可用）。SAFETY 同 [`Region::map_file`]。
    pub(crate) unsafe fn map_anonymous(&self, k: usize) -> io::Result<()> {
        let at = self.window(k);
        // SAFETY: 同上
        let p = unsafe { libc::mmap(at.cast(), WINDOW, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE, -1, 0) };
        if p == libc::MAP_FAILED { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// 窗口不用了：换回不可访问的保留（地址仍归本区）。SAFETY（调用方）：之后没人再访问这个窗口。
    pub(crate) unsafe fn unmap(&self, k: usize) {
        let at = self.window(k);
        // SAFETY: 见上
        unsafe { libc::mmap(at.cast(), WINDOW, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE, -1, 0) };
    }

    /// 把文件延长到至少覆盖 `[offset, offset + len)`（只会变长；多个线程同时调也安全）。
    pub(crate) fn commit(file: &File, offset: usize, len: usize) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let (offset, len) = (i64::try_from(offset).expect("窗口内偏移"), i64::try_from(len).expect("窗口内长度"));
        // SAFETY: 只改文件长度与分配
        if unsafe { libc::fallocate(file.as_raw_fd(), 0, offset, len) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
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

    pub(crate) fn reserve() -> io::Result<Self> {
        // SAFETY: 大小非零
        let p = unsafe { std::alloc::alloc_zeroed(Self::LAYOUT) };
        Ok(Self { base: NonNull::new(p).unwrap_or_else(|| std::alloc::handle_alloc_error(Self::LAYOUT)) })
    }

    pub(crate) unsafe fn map_file(&self, _: usize, _: &File) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub(crate) unsafe fn map_anonymous(&self, _: usize) -> io::Result<()> {
        Ok(())
    }

    pub(crate) unsafe fn unmap(&self, k: usize) {
        // SAFETY: 窗口在区内；之后没人再访问
        unsafe { self.window(k).write_bytes(0, WINDOW) };
    }

    pub(crate) fn commit(_: &File, _: usize, _: usize) -> io::Result<()> {
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
