//! Workload definitions and timing reports shared by every collector.
//!
//! One executable per collector: `bench-<name> <workload> <threads>`, printing one line `name\tworkload\tthreads\twall ms\tCPU ms`.
//! Threads: for pause / gcbench the number of collector threads, for alloc-mt the number of mutator threads (with as many collector threads).
use std::fmt;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::time::{Duration, Instant};

/// Pause: levels of the live full binary tree (plus an equally large garbage tree)
pub const PAUSE_DEPTH: u32 = 20;
/// GCBench: first build a temporary "stretch tree"
pub const STRETCH_DEPTH: u32 = 18;
/// GCBench: the tree alive throughout
pub const LONG_LIVED_DEPTH: u32 = 16;
/// GCBench: smallest / largest levels of the temporary trees (step 2)
pub const MIN_DEPTH: u32 = 4;
pub const MAX_DEPTH: u32 = 16;
/// GCBench: levels grow by 2 each time
const DEPTH_STEP: usize = 2;
/// Multi-threaded allocation: levels of each temporary tree, total number of trees, number of rounds (collecting as needed after each)
pub const MT_DEPTH: u32 = 10;
pub const MT_TREES: usize = 2048;
pub const MT_ROUNDS: usize = 16;

/// A full binary tree.
pub struct Tree;

impl Tree {
    /// Nodes in `depth` levels.
    pub const fn size(depth: u32) -> usize {
        (1 << depth) - 1
    }
}

/// GCBench's (Boehm's classic benchmark) iteration plan: (temporary tree levels, tree count), the same total nodes allocated per step.
pub struct GcBench;

impl GcBench {
    pub fn rounds() -> impl Iterator<Item = (u32, usize)> {
        (MIN_DEPTH..=MAX_DEPTH).step_by(DEPTH_STEP).map(|d| (d, 2 * Tree::size(STRETCH_DEPTH) / Tree::size(d)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Workload {
    /// Time of one full collection after 1 live tree + 1 garbage tree (2^20 − 1 nodes each)
    Pause,
    /// GCBench: heavy allocation of temporary trees + a long-lived tree, collecting at the library's own pace; total time
    GcBench,
    /// Several mutator threads building temporary trees at once; total time (fixed total work)
    AllocMt,
    /// As Pause, but marking concurrently first (untimed, mutators run as usual meanwhile), timing only the pause of the finishing collection
    PauseConcurrent,
    /// As Pause, with marking still in the pause and sweeping concurrent (untimed): times only the pause finishing marking
    PauseSweepConcurrent,
    /// Concurrent marking + concurrent sweeping: times only the pause finishing marking
    PauseAllConcurrent,
    /// Fragmentation: objects of four sizes with random lifetimes ([`Frag`]); measures total time and RSS after collection / at peak
    Frag,
}

impl Workload {
    const NAMES: [(Self, &str); 7] = [
        (Self::Pause, "pause"),
        (Self::GcBench, "gcbench"),
        (Self::AllocMt, "alloc-mt"),
        (Self::PauseConcurrent, "pause-cm"),
        (Self::PauseSweepConcurrent, "pause-cs"),
        (Self::PauseAllConcurrent, "pause-cms"),
        (Self::Frag, "frag"),
    ];

    fn name(self) -> &'static str {
        Self::NAMES.iter().find(|(w, _)| *w == self).map_or("?", |(_, n)| n)
    }
}

impl FromStr for Workload {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Self::NAMES.iter().find(|(_, n)| *n == s).map(|(w, _)| *w).ok_or_else(|| format!("unknown workload {s} (pause / gcbench / alloc-mt / pause-cm / pause-cs / pause-cms / frag)"))
    }
}

impl fmt::Display for Workload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One measurement: wall time and the CPU time of the whole process (all threads). CPU ÷ wall ≈ the average number of threads working.
#[derive(Clone, Copy)]
pub struct Took {
    pub wall: Duration,
    pub cpu: Duration,
}

/// Start timing.
pub struct Clock {
    wall: Instant,
    cpu: Duration,
}

impl Clock {
    pub fn start() -> Self {
        Self { wall: Instant::now(), cpu: Self::process_cpu() }
    }

    pub fn stop(&self) -> Took {
        Took { wall: self.wall.elapsed(), cpu: Self::process_cpu().saturating_sub(self.cpu) }
    }

    /// CPU time used by the whole process so far (summed over all threads).
    fn process_cpu() -> Duration {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: ts is a valid output location
        let ok = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &raw mut ts) } == 0;
        assert!(ok, "clock_gettime(CLOCK_PROCESS_CPUTIME_ID) failed");
        Duration::new(u64::try_from(ts.tv_sec).unwrap_or(0), u32::try_from(ts.tv_nsec).unwrap_or(0))
    }
}

/// A collector's benchmarks; unsupported ones return None.
pub trait Bench {
    const NAME: &str;

    fn pause(&self) -> Option<Took>;
    fn gcbench(&self) -> Option<Took>;
    fn alloc_mt(&self) -> Option<Took> {
        None
    }
    fn pause_concurrent(&self) -> Option<Took> {
        None
    }
    fn pause_sweep_concurrent(&self) -> Option<Took> {
        None
    }
    fn pause_all_concurrent(&self) -> Option<Took> {
        None
    }
    fn frag(&self) -> Option<(Took, Footprint)> {
        None
    }
}

/// Command line: `<workload> <threads>`.
pub struct Args {
    pub workload: Workload,
    pub threads: NonZeroUsize,
}

impl Args {
    pub fn parse() -> Self {
        let mut argv = std::env::args().skip(1);
        let usage = "usage: bench-<name> <pause|gcbench|alloc-mt|pause-cm|pause-cs|pause-cms|frag> <threads>";
        let workload = argv.next().expect(usage).parse().unwrap_or_else(|e: String| panic!("{e}"));
        let threads = argv.next().expect(usage).parse().expect("the thread count is a positive integer");
        Self { workload, threads }
    }

    /// Run one benchmark and print a line of results; exit with code 2 when unsupported.
    pub fn report<B: Bench>(&self, bench: &B) {
        if self.workload == Workload::Frag {
            let Some((t, f)) = bench.frag() else {
                eprintln!("{} does not support {}", B::NAME, self.workload);
                std::process::exit(2);
            };
            println!(
                "{}\t{}\t{}\t{:.2}\t{:.2}\t{:.1}\t{:.1}",
                B::NAME,
                self.workload,
                self.threads,
                t.wall.as_secs_f64() * 1e3,
                t.cpu.as_secs_f64() * 1e3,
                Footprint::mb(f.after),
                Footprint::mb(f.peak)
            );
            return;
        }
        let took = match self.workload {
            Workload::Frag => unreachable!("handled above"),
            Workload::Pause => bench.pause(),
            Workload::GcBench => bench.gcbench(),
            Workload::AllocMt => bench.alloc_mt(),
            Workload::PauseConcurrent => bench.pause_concurrent(),
            Workload::PauseSweepConcurrent => bench.pause_sweep_concurrent(),
            Workload::PauseAllConcurrent => bench.pause_all_concurrent(),
        };
        match took {
            Some(t) => println!("{}\t{}\t{}\t{:.2}\t{:.2}", B::NAME, self.workload, self.threads, t.wall.as_secs_f64() * 1e3, t.cpu.as_secs_f64() * 1e3),
            None => {
                eprintln!("{} does not support {}", B::NAME, self.workload);
                std::process::exit(2);
            }
        }
    }
}

/// Fragmentation load: one group of slots per size (in the holder); each round allocates [`FRAG_PER_ROUND`] objects and randomly overwrites old objects in the slots at a different rate per size
/// (overwritten ones become garbage ⇒ random lifetimes, with live objects scattered everywhere). About 80 MB live.
pub const FRAG_SLOTS: usize = 200_000;
/// Objects allocated per round
pub const FRAG_PER_ROUND: usize = 2 * FRAG_SLOTS;
/// Rounds
pub const FRAG_ROUNDS: usize = 30;

/// Object sizes in the fragmentation load (data bytes, excluding each library's header).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Size {
    /// 24 bytes, 70 % of allocations, 1/7 kept in a slot
    Small,
    /// 240 bytes, 20 %, 1/5 kept
    Medium,
    /// 1600 bytes, 8 %, 1/3 kept
    Large,
    /// 12000 bytes, 2 %, 1/2 kept
    Huge,
}

impl Size {
    /// Slots of this size
    pub const fn slots(self) -> usize {
        match self {
            Self::Small => FRAG_SLOTS,
            Self::Medium => FRAG_SLOTS / 4,
            Self::Large => FRAG_SLOTS / 16,
            Self::Huge => FRAG_SLOTS / 64,
        }
    }
}

/// One allocation: the size, and whether and into which slot it goes.
pub struct Pick {
    pub size: Size,
    pub slot: Option<usize>,
}

/// The deterministic random sequence of the fragmentation load (xorshift64).
pub struct Frag(u64);

impl Default for Frag {
    fn default() -> Self {
        Self(0x9e37_79b9_7f4a_7c15)
    }
}

impl Frag {
    pub fn next(&mut self) -> Pick {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        let (size, keep_one_in) = match x % 100 {
            0..70 => (Size::Small, 7),
            70..90 => (Size::Medium, 5),
            90..98 => (Size::Large, 3),
            _ => (Size::Huge, 2),
        };
        let slot = (x % keep_one_in == 0).then(|| usize::try_from(x / 100).unwrap_or(0) % size.slots());
        Pick { size, slot }
    }
}

/// Memory of the fragmentation load: RSS after collection and at peak (bytes, minus the starting value).
#[derive(Clone, Copy, Default)]
pub struct Footprint {
    pub after: usize,
    pub peak: usize,
}

impl Footprint {
    /// Current RSS (bytes).
    pub fn rss() -> usize {
        let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
        statm.split_whitespace().nth(1).and_then(|p| p.parse::<usize>().ok()).unwrap_or(0) * PAGE_BYTES
    }

    fn mb(bytes: usize) -> f64 {
        bytes as f64 / 1e6
    }
}

/// The unit of /proc/self/statm
const PAGE_BYTES: usize = 4096;
