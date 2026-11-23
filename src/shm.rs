//! Shared memory and processes (Linux only): the public region and window mappings [`Region`], [`CrossHeapSpace`], heap-number registration in a directory [`Registry`], process liveness [`Process`].
//!
//! The public region: every process reserves an equally sized range of addresses aligned to its own size and cuts it into fixed-size windows by heap number; public heap k is always mapped at window k.
//!
//! Reserving takes only address space (`PROT_NONE` + `MAP_NORESERVE`), no memory. Miri cannot map files, so there it is a zeroed allocation with the same alignment;
//! the public heap logic can still be tested (only without files, so not across processes).
//!
//! Two things needed across processes: identifying a process (pid + start time, so a reused pid is not mistaken for the old one) and futex wait / wake on shared memory.
//!
//! Public heap registration in a directory: claiming heap numbers (`.paragc-<number>` marker files) and judging liveness.
//!
//! A public heap dies with its owner (the creating process): when the owner exits or is killed without persist, the heap is dead. Liveness reads only a few fields at the start of the heap file's superblock,
//! without mapping the whole heap, so a collector can judge a heap it never opened; once judged dead, the heap file and marker are deleted (the file's memory returns to the system when the last process mapping it lets go).
//!
//! Claiming numbers, judging staleness and deleting markers all happen under the marker file's `flock` (after locking, check that the path is still this file and was not deleted and recreated):
//! two-step "look, then delete" sequences never interleave with someone else's claim. The kernel releases the lock when a process is killed.
//!
//! [`CrossHeapSpace`]: this process's public region; public heaps are created and opened on it.
#![cfg(crossheap)]
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

use crate::heap::{Backing, CrossHeap, CrossHeapInternal, Inner};
use crate::layout::{MAGIC, Super};

/// Size of one window (all the addresses of one public heap)
pub const WINDOW: usize = if cfg!(miri) { 16 << 20 } else { 16 << 30 };
/// Number of windows (the limit on public heap numbers)
pub const WINDOWS: usize = if cfg!(miri) { 4 } else { 64 };
/// The whole public region; aligned to its size, so masking the low bits of any address inside gives the region's start
pub const REGION: usize = WINDOW * WINDOWS;

const _: () = assert!(REGION.is_power_of_two() && WINDOW.is_power_of_two());

/// The fixed-address public region sits here (the same in every process, so absolute addresses stored in objects are valid in every process): at 64 TiB, aligned to the region size,
/// away from where executables, the heap and mmap usually go on x86-64 / AArch64 (48-bit virtual addresses)
pub const FIXED_BASE: usize = 64 << 40;

const _: () = assert!(FIXED_BASE.is_multiple_of(REGION));

pub struct Region {
    base: NonNull<u8>,
}

// SAFETY: just a range of addresses; its contents are synchronized by the public heaps themselves
unsafe impl Send for Region {}
// SAFETY: as above
unsafe impl Sync for Region {}

impl Region {
    /// `addr` lies in the fixed-address public region ([`FIXED_BASE`]). There is no fixed region under Miri.
    #[inline]
    pub fn holds_fixed(addr: usize) -> bool {
        !cfg!(miri) && addr & !(REGION - 1) == FIXED_BASE
    }

    /// The start of the region containing `p` (low bits masked off).
    pub fn base_of(p: usize) -> usize {
        p & !(REGION - 1)
    }

    pub fn base(&self) -> NonNull<u8> {
        self.base
    }

    /// The start of window `k`.
    pub fn window(&self, k: usize) -> *mut u8 {
        self.base.as_ptr().wrapping_add(k * WINDOW)
    }
}

#[cfg(not(miri))]
impl Region {
    /// Reserve the whole region: reserve twice as much to find an aligned start, then return the excess at both ends.
    pub fn reserve() -> io::Result<Self> {
        // SAFETY: a new anonymous inaccessible mapping that touches no existing memory
        let raw = unsafe { libc::mmap(core::ptr::null_mut(), 2 * REGION, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0) };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let raw = raw.cast::<u8>();
        let head = raw.align_offset(REGION);
        let base = raw.wrapping_add(head);
        // SAFETY: both pieces are inside the mapping just reserved and outside the kept [base, base + REGION)
        unsafe {
            if head > 0 {
                libc::munmap(raw.cast(), head);
            }
            libc::munmap(base.wrapping_add(REGION).cast(), REGION - head);
        }
        Ok(Self { base: NonNull::new(base).expect("mmap never returns null") })
    }

    /// Reserve the whole region at [`FIXED_BASE`]; fails if something is already mapped there (or the address space is too small).
    pub fn reserve_fixed() -> io::Result<Self> {
        let want = core::ptr::without_provenance_mut::<libc::c_void>(FIXED_BASE);
        // SAFETY: MAP_FIXED_NOREPLACE does not replace existing mappings
        let raw = unsafe { libc::mmap(want, REGION, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE | libc::MAP_FIXED_NOREPLACE, -1, 0) };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        if raw.addr() != FIXED_BASE {
            // Old kernels ignore MAP_FIXED_NOREPLACE and treat the address as a hint: give it back
            // SAFETY: just mapped
            unsafe { libc::munmap(raw, REGION) };
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "the fixed-address public region is taken"));
        }
        Ok(Self { base: NonNull::new(raw.cast()).expect("mmap never returns null") })
    }

    /// Map a file at window `k` (the whole window at once; when the file is shorter only the extended part is accessed).
    /// SAFETY (caller): window `k` is not in use.
    pub unsafe fn map_file(&self, k: usize, file: &File) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let at = self.window(k);
        // SAFETY: see above; MAP_FIXED only replaces this region's own reserved range
        let p = unsafe { libc::mmap(at.cast(), WINDOW, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_FIXED, file.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// A public heap without a file: the window becomes shared anonymous memory (usable within one process). SAFETY as for [`Region::map_file`].
    pub unsafe fn map_anonymous(&self, k: usize) -> io::Result<()> {
        let at = self.window(k);
        // SAFETY: as above
        let p = unsafe { libc::mmap(at.cast(), WINDOW, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE, -1, 0) };
        if p == libc::MAP_FAILED { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// The window is no longer used: turn it back into an inaccessible reservation (the addresses still belong to the region). SAFETY (caller): nobody accesses the window afterwards.
    pub unsafe fn unmap(&self, k: usize) {
        let at = self.window(k);
        // SAFETY: see above
        unsafe { libc::mmap(at.cast(), WINDOW, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE, -1, 0) };
    }

    /// Extend the file to cover at least `[offset, offset + len)` (it only grows; safe when several threads call at once).
    pub fn commit(file: &File, offset: usize, len: usize) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let (offset, len) = (i64::try_from(offset).expect("offset within the window"), i64::try_from(len).expect("length within the window"));
        // SAFETY: only changes the file's length and allocation
        if unsafe { libc::fallocate(file.as_raw_fd(), 0, offset, len) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    /// Punch a hole in the file over `[offset, offset + len)` (the length stays; it reads as zeros afterwards). Best effort: left alone on failure.
    pub fn punch(file: &File, offset: usize, len: usize) {
        use std::os::fd::AsRawFd;
        let (Ok(offset), Ok(len)) = (i64::try_from(offset), i64::try_from(len)) else { return };
        // SAFETY: only discards this part of the file's contents
        unsafe { libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE, offset, len) };
    }

    /// Give `[at, at + len)` of shared anonymous memory back to the system (it reads as zeros afterwards). Best effort.
    pub fn remove(at: *mut u8, len: usize) {
        // SAFETY: the caller passes a range within one of this region's windows that holds nothing still needed
        unsafe { libc::madvise(at.cast(), len, libc::MADV_REMOVE) };
    }
}

#[cfg(not(miri))]
impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the whole region belongs to it, and the heaps in its windows are all dropped (they hold the region's Arc)
        unsafe { libc::munmap(self.base.as_ptr().cast(), REGION) };
    }
}

#[cfg(miri)]
impl Region {
    const LAYOUT: std::alloc::Layout = match std::alloc::Layout::from_size_align(REGION, REGION) {
        Ok(l) => l,
        Err(_) => panic!("invalid region size / alignment"),
    };

    pub fn reserve() -> io::Result<Self> {
        // SAFETY: the size is non-zero
        let p = unsafe { std::alloc::alloc_zeroed(Self::LAYOUT) };
        Ok(Self { base: NonNull::new(p).unwrap_or_else(|| std::alloc::handle_alloc_error(Self::LAYOUT)) })
    }

    pub fn reserve_fixed() -> io::Result<Self> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub unsafe fn map_file(&self, _: usize, _: &File) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub unsafe fn map_anonymous(&self, _: usize) -> io::Result<()> {
        Ok(())
    }

    pub unsafe fn unmap(&self, k: usize) {
        // SAFETY: the window is inside the region; nobody accesses it afterwards
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
        // SAFETY: from reserve's alloc_zeroed
        unsafe { std::alloc::dealloc(self.base.as_ptr(), Self::LAYOUT) };
    }
}

/// A process: pid + start time (clock ticks since boot, field 22 of `/proc/<pid>/stat`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub start: u64,
}

/// In `/proc/<pid>/stat` after the closing parenthesis: field 3 (state) is number 0 and field 22 (start time) is number 19
const STATE_FIELD: usize = 0;
const START_FIELD: usize = 19;

impl Process {
    /// This process.
    pub fn current() -> Self {
        let pid = std::process::id();
        Self { pid, start: Self::stat(pid).map_or(0, |(_, start)| start) }
    }

    /// (state character, start time); None if the process is gone. Does not read /proc under Miri.
    #[cfg(not(miri))]
    fn stat(pid: u32) -> Option<(char, u64)> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The process name is in parentheses and may contain spaces, so count from after the last closing parenthesis
        let rest = &text[text.rfind(')')? + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        Some((fields.get(STATE_FIELD)?.chars().next()?, fields.get(START_FIELD)?.parse().ok()?))
    }

    #[cfg(miri)]
    fn stat(_: u32) -> Option<(char, u64)> {
        None
    }

    /// Still alive: present in /proc, not a zombie, and the start time matches. A recorded start time of 0 (still registering, or /proc unreadable) always counts as alive.
    pub fn alive(self) -> bool {
        if self.start == 0 || cfg!(miri) {
            return true;
        }
        Self::stat(self.pid).is_some_and(|(state, start)| start == self.start && !matches!(state, 'Z' | 'X'))
    }
}

/// Marker file name prefix: `.paragc-<number>`; the content is one line "creator pid start time" + the heap file name (empty = the number is free)
const MARKER_PREFIX: &str = ".paragc-";

/// Whether a heap is still there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Liveness {
    Alive,
    /// Dead (owner gone, no persist); file and marker already deleted
    Dead,
    /// The heap looked for (by identity) is gone: the number now belongs to another heap, or is free
    Gone,
    /// Cannot tell (error reading files and so on): treat it as alive
    Unknown,
}

/// A few fields from the start of a heap file's superblock.
pub struct FileHeader {
    pub heap: usize,
    pub incarnation: u64,
    dead: bool,
    persist: bool,
    owner: Process,
}

impl FileHeader {
    /// Read a heap file's superblock; an error when it is not a paragc public heap (magic, window size or heap number wrong).
    pub fn read(file: &File) -> io::Result<Self> {
        let mut buf = [0u8; size_of::<Super>()];
        file.read_exact_at(&mut buf, 0)?;
        let u64_at = |o: usize| u64::from_le_bytes(buf[o..o + size_of::<u64>()].try_into().expect("8 bytes"));
        let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + size_of::<u32>()].try_into().expect("4 bytes"));
        if u64_at(offset_of!(Super, magic)) != MAGIC || u64_at(offset_of!(Super, window)) != WINDOW as u64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a paragc public heap, or a different layout version"));
        }
        let heap = usize::try_from(u64_at(offset_of!(Super, heap))).ok().filter(|&k| k < WINDOWS).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "heap number beyond the window count"))?;
        Ok(Self {
            heap,
            incarnation: u64_at(offset_of!(Super, incarnation)),
            dead: u32_at(offset_of!(Super, dead)) != 0,
            persist: u32_at(offset_of!(Super, persist)) != 0,
            owner: Process { pid: u32_at(offset_of!(Super, owner_pid)), start: u64_at(offset_of!(Super, owner_start)) },
        })
    }

    /// This heap is dead: marked dead, or the owner is gone without persist.
    pub fn is_dead(&self) -> bool {
        self.dead || (!self.persist && !self.owner.alive())
    }
}

/// Contents of a marker file.
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

/// Public heap registration in one directory.
pub struct Registry<'d> {
    dir: &'d Path,
}

impl<'d> Registry<'d> {
    /// Registration of the directory containing heap file `path`.
    pub fn of(path: &'d Path) -> Self {
        Self { dir: path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new(".")) }
    }

    pub fn dir(&self) -> &Path {
        self.dir
    }

    pub fn marker(&self, k: usize) -> PathBuf {
        self.dir.join(format!("{MARKER_PREFIX}{k}"))
    }

    /// Open and lock the marker of heap number `k` (`create`: make an empty one if missing; otherwise NotFound when missing); after locking the path must still be this file,
    /// otherwise (someone deleted it) start over. The lock is released when the file is closed.
    fn lock(&self, k: usize, create: bool) -> io::Result<File> {
        use std::os::fd::AsRawFd;
        let path = self.marker(k);
        loop {
            let f = OpenOptions::new().read(true).write(true).create(create).truncate(false).mode(0o600).open(&path)?;
            // SAFETY: the fd is valid; flock affects only this open file
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

    /// Read the marker contents under the lock.
    fn read_locked(f: &mut File) -> io::Result<Vec<u8>> {
        let mut text = Vec::new();
        f.rewind()?;
        f.read_to_end(&mut text)?;
        Ok(text)
    }

    /// Claim heap number `k` (recording this process and heap file name `name`); Ok(false) when the number is held by a heap that is alive (or being created). Stale markers are reclaimed under the lock.
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

    /// Judge the heap on number `k` from its marker contents (deleting nothing). `expect`: the identity looked for.
    ///
    /// Creating a heap takes several steps (claim the number, create the file, extend it, write the superblock), and other processes may read any intermediate state: as long as the creating process lives,
    /// anything incomplete (no file yet, a superblock that is still zero or half written) counts as being created.
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

    /// Whether the heap on number `k` is still there. `expect`: which identity is looked for (None = whatever heap holds the number). Deletes the heap file and marker when it is judged dead.
    pub fn liveness(&self, k: usize, expect: Option<u64>) -> Liveness {
        let text = match std::fs::read(self.marker(k)) {
            Ok(t) => t,
            // The marker is gone: no heap on this number (it died and was deleted)
            Err(e) if e.kind() == io::ErrorKind::NotFound => return if expect.is_some() { Liveness::Gone } else { Liveness::Dead },
            Err(_) => return Liveness::Unknown,
        };
        let Some(marker) = Marker::parse(&text) else {
            // An empty marker (the number is free) or one being written
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

    /// Delete the file and marker of the heap with number `k` and identity `incarnation`: under the marker's lock, check that the marker still points at this file and the file still has this identity before deleting
    /// (the number may have been reused meanwhile).
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

    /// Creating a heap failed: give back the number just claimed (the marker is deleted only if this process claimed it for `name`).
    pub fn abandon(&self, k: usize, name: &OsStr) {
        let Ok(mut f) = self.lock(k, false) else { return };
        let Ok(text) = Self::read_locked(&mut f) else { return };
        if Marker::parse(&text).is_some_and(|m| m.name == name.as_encoded_bytes() && m.creator == Some(Process::current())) {
            let _ = std::fs::remove_file(self.marker(k));
        }
    }
}

const _: () = assert!(WINDOWS <= u64::BITS as usize, "the window bitmap is one u64");



pub struct Space {
    region: Region,
    /// The region sits at [`FIXED_BASE`]
    fixed: bool,
    /// Bitmap of windows in use
    used: AtomicU64,
}

impl Space {
    pub fn region(&self) -> &Region {
        &self.region
    }

    pub fn is_fixed(&self) -> bool {
        self.fixed
    }

    /// Take window `k`; false if it is already taken.
    fn claim(&self, k: usize) -> bool {
        self.used.fetch_or(1 << k, Ordering::AcqRel) & (1 << k) == 0
    }

    /// Take a free window.
    fn claim_any(&self) -> Option<usize> {
        (0..WINDOWS).find(|&k| self.claim(k))
    }

    pub fn release(&self, k: usize) {
        self.used.fetch_and(!(1 << k), Ordering::AcqRel);
    }
}

/// This process's public region: a reserved range of addresses (no memory) cut into windows by heap number; public heap k is mapped at window k,
/// so offsets stored in a public heap point at the same object in every process. A process usually needs one.
#[derive(Clone)]
pub struct CrossHeapSpace(Arc<Space>);

impl CrossHeapSpace {
    pub fn reserve() -> io::Result<Self> {
        Ok(Self(Arc::new(Space { region: Region::reserve()?, fixed: false, used: AtomicU64::new(0) })))
    }

    /// Reserve the public region at the fixed address ([`FIXED_BASE`], the same in every process): objects may store absolute addresses ([`crate::ScopeLocal`], [`crate::Slot`],
    /// [`crate::WordSlot`]) and other processes read them correctly. A private heap that promotes young objects into a public heap ([`crate::Heap::attach_cross_heap`]) needs such a region;
    /// a process can have only one. Fails if the address is taken.
    pub fn reserve_fixed() -> io::Result<Self> {
        Ok(Self(Arc::new(Space { region: Region::reserve_fixed()?, fixed: true, used: AtomicU64::new(0) })))
    }

    /// Create a public heap at `path` (a new file on a shared-memory file system, such as `/dev/shm/…` or your own tmpfs / hugetlbfs mount).
    /// The file starts empty and is extended as it is used; mode 0600, so only processes with permission on the file can open it.
    ///
    /// Heap numbers are unique across processes within a directory: they are claimed under the lock of a `.paragc-<number>` marker file (reclaimed when the heap it names is dead).
    /// This process owns the heap: when this handle (and every [`crate::CrossHeapGlobal`] made from it) is dropped, or this process exits / is killed, the heap dies,
    /// unless [`CrossHeap::persist`] was called.
    pub fn create<R>(&self, path: impl AsRef<Path>, threads: NonZeroUsize, f: impl for<'xheap> FnOnce(CrossHeap<'xheap>) -> R) -> io::Result<R> {
        self.create_heap(path.as_ref(), threads).map(|h| f(CrossHeap::rebrand(h)))
    }

    fn create_heap(&self, path: &Path, threads: NonZeroUsize) -> io::Result<CrossHeap<'static>> {
        use std::os::unix::fs::OpenOptionsExt;
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

    /// Claim a heap number: the window in this region is free and `.paragc-<number>` can be claimed in the heap file's directory.
    fn claim_number(&self, path: &Path) -> io::Result<usize> {
        let registry = Registry::of(path);
        let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the heap file path has no file name"))?;
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
        Err(io::Error::new(io::ErrorKind::OutOfMemory, "out of public heap numbers (markers in this directory or windows in this region)"))
    }

    /// A public heap without a file (used only within this process; for tests and Miri).
    pub fn create_anonymous<R>(&self, threads: NonZeroUsize, f: impl for<'xheap> FnOnce(CrossHeap<'xheap>) -> R) -> io::Result<R> {
        self.create_anonymous_heap(threads).map(|h| f(CrossHeap::rebrand(h)))
    }

    fn create_anonymous_heap(&self, threads: NonZeroUsize) -> io::Result<CrossHeap<'static>> {
        let k = self.0.claim_any().ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "out of windows in the public region"))?;
        self.attach(k, Backing::Anonymous, threads, true)
    }

    /// Open a public heap created by someone else (or by another region of this process), mapped at the window of its heap number.
    pub fn open<R>(&self, path: impl AsRef<Path>, threads: NonZeroUsize, f: impl for<'xheap> FnOnce(CrossHeap<'xheap>) -> R) -> io::Result<R> {
        self.open_heap(path.as_ref(), threads).map(|h| f(CrossHeap::rebrand(h)))
    }

    fn open_heap(&self, path: &Path, threads: NonZeroUsize) -> io::Result<CrossHeap<'static>> {
        let path = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        self.open_file(file, Some(path), threads)
    }

    /// Open a public heap from a file descriptor (a [`CrossHeap::fd`] passed by another process through `SCM_RIGHTS`), without access to the heap file's path.
    pub fn open_fd<R>(&self, fd: std::os::fd::OwnedFd, threads: NonZeroUsize, f: impl for<'xheap> FnOnce(CrossHeap<'xheap>) -> R) -> io::Result<R> {
        self.open_fd_heap(fd, threads).map(|h| f(CrossHeap::rebrand(h)))
    }

    fn open_fd_heap(&self, fd: std::os::fd::OwnedFd, threads: NonZeroUsize) -> io::Result<CrossHeap<'static>> {
        self.open_file(File::from(fd), None, threads)
    }

    fn open_file(&self, file: File, path: Option<&Path>, threads: NonZeroUsize) -> io::Result<CrossHeap<'static>> {
        let header = FileHeader::read(&file)?;
        if header.is_dead() {
            if let Some(path) = path {
                Registry::of(path).bury(path, header.heap, header.incarnation);
            }
            return Err(io::Error::new(io::ErrorKind::NotFound, "this public heap is dead (its owner is gone)"));
        }
        let k = header.heap;
        if !self.0.claim(k) {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "this public heap's window is already taken in this region"));
        }
        self.attach(k, Backing::File { file, path: path.map(Path::to_path_buf), owner: false }, threads, false)
    }

    /// Map `backing` at window `k`, writing a new superblock when `fresh`. Gives the window back on failure.
    fn attach(&self, k: usize, backing: Backing, threads: NonZeroUsize, fresh: bool) -> io::Result<CrossHeap<'static>> {
        // SAFETY: window k was just taken and is unused
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
