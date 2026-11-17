//! 跨进程要用的两样：认进程（进程号 + 启动时间，防进程号被复用后认错人）、共享内存上的 futex 等待 / 唤醒。
use core::sync::atomic::AtomicU32;
use core::time::Duration;

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

/// 共享内存上的 32 位字当 futex 用（不带 FUTEX_PRIVATE_FLAG，跨进程有效）。Miri 下退化成让出时间片。
pub trait FutexWord {
    /// 字还等于 `expected` 就睡，最多 `timeout`（醒来不保证字变了，调用方自己再查）。
    fn wait(&self, expected: u32, timeout: Duration);
    /// 叫醒在这个字上睡的全部线程（任何进程里的）。
    fn wake_all(&self);
}

#[cfg(not(miri))]
impl FutexWord for AtomicU32 {
    fn wait(&self, expected: u32, timeout: Duration) {
        let ts = libc::timespec {
            tv_sec: libc::time_t::try_from(timeout.as_secs()).unwrap_or(libc::time_t::MAX),
            tv_nsec: timeout.subsec_nanos().into(),
        };
        // SAFETY: 字在共享内存里一直有效；FUTEX_WAIT 只读它
        unsafe { libc::syscall(libc::SYS_futex, self.as_ptr(), libc::FUTEX_WAIT, expected, &raw const ts) };
    }

    fn wake_all(&self) {
        // SAFETY: 同上
        unsafe { libc::syscall(libc::SYS_futex, self.as_ptr(), libc::FUTEX_WAKE, i32::MAX) };
    }
}

#[cfg(miri)]
impl FutexWord for AtomicU32 {
    fn wait(&self, _: u32, _: Duration) {
        std::thread::yield_now();
    }

    fn wake_all(&self) {}
}
