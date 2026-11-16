//! 并行标记：每个工人一个 Chase–Lev 双端队列，自己的从底端弹，空了去偷别人的顶端。
//!
//! 结束判定：工人自己的队列空、也偷不到时把空闲计数加一，然后边等边偷；空闲计数等于工人数时
//! 所有队列都空、也没人还在追踪（追踪中的工人没计入空闲），标记结束。偷到了就先减一再干活；
//! 偷到还没来得及减一时别人可能先看到「全空闲」退出，但手里的活仍由偷到的人做完，结果不受影响。
use core::sync::atomic::{AtomicUsize, Ordering};

use j8lockfree::Deque;

use crate::header::Header;
use crate::trace::Tracer;
use crate::workers::{Backoff, Workers};

pub(crate) struct Marking {
    queues: Vec<Deque<usize>>,
    idle: AtomicUsize,
    epoch: u32,
}

impl Marking {
    pub(crate) fn new(workers: Workers, epoch: u32) -> Self {
        Self { queues: (0..workers.count()).map(|_| Deque::default()).collect(), idle: AtomicUsize::new(0), epoch }
    }

    /// 从根出发标记全部可达对象（标记位 + 行标记）。
    pub(crate) fn run(&self, workers: Workers, roots: &[*const Header]) {
        // 根轮流分给各工人的队列
        for (q, chunk) in self.queues.iter().zip(roots.chunks(roots.len().div_ceil(self.queues.len()).max(1))) {
            let owner = q.owner().expect("还没有工人领队列");
            let mut t = Tracer::new(&owner, self.epoch);
            for &root in chunk {
                t.visit(root, false);
            }
        }
        workers.run(|i| self.work(i));
    }

    fn work(&self, me: usize) {
        let owner = self.queues[me].owner().expect("每个队列只有一个工人");
        let mut t = Tracer::new(&owner, self.epoch);
        loop {
            while let Some(item) = owner.pop() {
                t.scan(item);
            }
            if let Some(item) = self.steal(me) {
                t.scan(item);
                continue;
            }
            self.idle.fetch_add(1, Ordering::AcqRel);
            match self.wait(me) {
                Some(item) => t.scan(item),
                None => return,
            }
        }
    }

    /// 空闲时边等边偷；全部空闲返回 None。
    fn wait(&self, me: usize) -> Option<usize> {
        let mut backoff = Backoff::default();
        loop {
            if self.idle.load(Ordering::Acquire) == self.queues.len() {
                return None;
            }
            if let Some(item) = self.steal(me) {
                self.idle.fetch_sub(1, Ordering::AcqRel);
                return Some(item);
            }
            backoff.snooze();
        }
    }

    /// 从下一个工人开始轮着偷一个。
    fn steal(&self, me: usize) -> Option<usize> {
        let n = self.queues.len();
        (1..n).find_map(|k| self.queues[(me + k) % n].steal())
    }
}
