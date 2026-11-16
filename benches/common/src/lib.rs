//! 各回收器共用的工作负载定义与计时报告。
//!
//! 每个回收器一个可执行文件：`bench-<名字> <工作负载> <线程数>`，打印一行 `名字\t工作负载\t线程数\t毫秒`。
//! 线程数：pause / gcbench 是回收线程数，alloc-mt 是 mutator 线程数（回收线程同数）。
use std::fmt;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::time::Duration;

/// 停顿：活着的满二叉树层数（另造同样大的一棵垃圾）
pub const PAUSE_DEPTH: u32 = 20;
/// GCBench：先造一棵临时的「拉伸树」
pub const STRETCH_DEPTH: u32 = 18;
/// GCBench：整个过程中一直活着的树
pub const LONG_LIVED_DEPTH: u32 = 16;
/// GCBench：临时树的最小 / 最大层数（步长 2）
pub const MIN_DEPTH: u32 = 4;
pub const MAX_DEPTH: u32 = 16;
/// GCBench：层数每次加 2
const DEPTH_STEP: usize = 2;
/// 多线程分配：每棵临时树的层数、总棵数、分几轮（每轮之后按需回收）
pub const MT_DEPTH: u32 = 10;
pub const MT_TREES: usize = 2048;
pub const MT_ROUNDS: usize = 16;

/// 满二叉树。
pub struct Tree;

impl Tree {
    /// `depth` 层的节点数。
    pub const fn size(depth: u32) -> usize {
        (1 << depth) - 1
    }
}

/// GCBench（Boehm 的经典基准）的迭代计划：(临时树层数, 棵数)，每档分配的节点总数相同。
pub struct GcBench;

impl GcBench {
    pub fn rounds() -> impl Iterator<Item = (u32, usize)> {
        (MIN_DEPTH..=MAX_DEPTH).step_by(DEPTH_STEP).map(|d| (d, 2 * Tree::size(STRETCH_DEPTH) / Tree::size(d)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Workload {
    /// 1 棵活树 + 1 棵垃圾树（各 2^20 − 1 个节点）后，一次完整回收的耗时
    Pause,
    /// GCBench：临时树大量分配 + 一棵长寿树，按库自己的节奏回收，总耗时
    GcBench,
    /// 多个 mutator 线程同时造临时树，总耗时（总工作量固定）
    AllocMt,
}

impl Workload {
    const NAMES: [(Self, &str); 3] = [(Self::Pause, "pause"), (Self::GcBench, "gcbench"), (Self::AllocMt, "alloc-mt")];

    fn name(self) -> &'static str {
        Self::NAMES.iter().find(|(w, _)| *w == self).map_or("?", |(_, n)| n)
    }
}

impl FromStr for Workload {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Self::NAMES.iter().find(|(_, n)| *n == s).map(|(w, _)| *w).ok_or_else(|| format!("未知工作负载 {s}（pause / gcbench / alloc-mt）"))
    }
}

impl fmt::Display for Workload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// 一个回收器的三项基准；不支持的返回 None。
pub trait Bench {
    const NAME: &str;

    fn pause(&self) -> Option<Duration>;
    fn gcbench(&self) -> Option<Duration>;
    fn alloc_mt(&self) -> Option<Duration> {
        None
    }
}

/// 命令行：`<工作负载> <线程数>`。
pub struct Args {
    pub workload: Workload,
    pub threads: NonZeroUsize,
}

impl Args {
    pub fn parse() -> Self {
        let mut argv = std::env::args().skip(1);
        let usage = "用法：bench-<名字> <pause|gcbench|alloc-mt> <线程数>";
        let workload = argv.next().expect(usage).parse().unwrap_or_else(|e: String| panic!("{e}"));
        let threads = argv.next().expect(usage).parse().expect("线程数是正整数");
        Self { workload, threads }
    }

    /// 跑一项并打印一行结果；不支持时以退出码 2 结束。
    pub fn report<B: Bench>(&self, bench: &B) {
        let took = match self.workload {
            Workload::Pause => bench.pause(),
            Workload::GcBench => bench.gcbench(),
            Workload::AllocMt => bench.alloc_mt(),
        };
        match took {
            Some(d) => println!("{}\t{}\t{}\t{:.2}", B::NAME, self.workload, self.threads, d.as_secs_f64() * 1e3),
            None => {
                eprintln!("{} 不支持 {}", B::NAME, self.workload);
                std::process::exit(2);
            }
        }
    }
}
