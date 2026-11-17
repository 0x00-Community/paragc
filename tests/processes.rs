//! 多个进程挂同一个公开堆。子进程是本测试程序自己：带上 [`ROLE`] 等环境变量、只跑 `child` 那一条。
//! 逐条跑：`cargo test --test processes -- --exact <名字>`
#![cfg(all(target_os = "linux", not(miri)))]

use std::io::{BufRead, BufReader};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use paragc::{Public, PublicHeap, PublicSlot, PublicSpace, Shared, public_brand};

const THREADS: NonZeroUsize = NonZeroUsize::new(4).unwrap();
/// 公开堆的对象头
const HEADER: usize = 16;

/// 子进程的环境变量：做什么、堆文件、锚对象的入口号、用哪条链、跑几轮
const ROLE: &str = "PARAGC_CHILD";
const PATH: &str = "PARAGC_PATH";
const ANCHOR: &str = "PARAGC_ANCHOR";
const CHAIN: &str = "PARAGC_CHAIN";
const ROUNDS: &str = "PARAGC_ROUNDS";
/// 子进程准备好时打印这一行
const READY: &str = "PARAGC-READY";
/// 子进程跑完时打印这个前缀 + 链长
const DONE: &str = "PARAGC-LEN ";
/// 每轮在链头压入的节点数
const PUSH: u64 = 200;
/// 杀掉的进程最迟这么久内要被收拾、回收要能做完
const DEADLINE: Duration = Duration::from_secs(10);

#[repr(C)]
#[derive(Shared)]
struct Node<'p> {
    value: u64,
    next: PublicSlot<'p, Node<'p>>,
}

public_brand!(NodeB = for<'p> Node<'p>);

/// 每条链一个头；0 号归父进程，其余给子进程
#[repr(C)]
#[derive(Shared)]
struct Anchor<'p> {
    heads: [PublicSlot<'p, Node<'p>>; 8],
}

public_brand!(AnchorB = for<'p> Anchor<'p>);

const NODE: usize = (HEADER + size_of::<Node<'static>>()).next_multiple_of(8);
const ANCHOR_BYTES: usize = (HEADER + size_of::<Anchor<'static>>()).next_multiple_of(8);

/// 子进程做什么。
#[derive(Clone, Copy)]
enum Role {
    /// 在自己的链上压入弹出 [`ROUNDS`] 轮（每 5 轮回收一次），打印链长退出
    Churn,
    /// 不停地压入弹出、回收，直到被杀
    ChurnForever,
    /// 建一条只有自己持有的链，然后停在 mutate 里不出来，直到被杀
    Hold,
    /// 不停地回收，直到被杀
    CollectForever,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Churn => "churn",
            Self::ChurnForever => "churn-forever",
            Self::Hold => "hold",
            Self::CollectForever => "collect-forever",
        }
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        [Self::Churn, Self::ChurnForever, Self::Hold, Self::CollectForever].into_iter().find(|r| r.name() == s).ok_or_else(|| format!("未知的子进程角色 {s}"))
    }
}

/// 链现在的长度：链上的值从头起是 len-1, len-2, …, 0（任何一步被杀都保持这个形状），头的值 + 1 就是长度。
fn length(heap: &PublicHeap, anchor: &Public<AnchorB>, chain: usize) -> u64 {
    heap.mutate(|m| anchor.get(m).heads[chain].get(m).map_or(0, |n| n.value + 1))
}

/// 在链头压入 `push` 个、再弹掉 `pop` 个；返回新长度。
fn churn(heap: &PublicHeap, anchor: &Public<AnchorB>, chain: usize, push: u64, pop: u64) -> u64 {
    heap.mutate(|m| {
        let top = &anchor.get(m).heads[chain];
        let len = top.get(m).map_or(0, |n| n.value + 1);
        for v in len..len + push {
            let n = m.alloc(Node { value: v, next: top.get(m).into() });
            top.set(m, Some(n));
        }
        for _ in 0..pop.min(len + push) {
            let first = top.get(m).expect("链不比弹掉的短");
            top.set(m, first.next.get(m));
        }
        top.get(m).map_or(0, |n| n.value + 1)
    })
}

/// 链的形状对：从头起 len-1, …, 0。返回长度。
fn check(heap: &PublicHeap, anchor: &Public<AnchorB>, chain: usize) -> u64 {
    heap.mutate(|m| {
        let mut cur = anchor.get(m).heads[chain].get(m);
        let len = cur.map_or(0, |n| n.value + 1);
        let mut expect = len;
        while let Some(n) = cur {
            expect -= 1;
            assert_eq!(n.value, expect, "第 {chain} 条链上的值错位：对象被提前回收或指针坏了");
            cur = n.next.get(m);
        }
        assert_eq!(expect, 0, "第 {chain} 条链断了");
        len
    })
}

/// 简单的线性同余随机数（种子取时间，失败时打印出来好复现）
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
        let seed = u64::from(now.subsec_nanos()) ^ now.as_secs() ^ u64::from(std::process::id()) << 32;
        eprintln!("随机种子 {seed}");
        Self(seed | 1)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// 父进程这一侧：堆文件、地址区、堆、锚。
struct Parent {
    path: PathBuf,
    heap: PublicHeap,
    anchor: Public<AnchorB>,
}

impl Parent {
    fn new(name: &str) -> Self {
        let path = PathBuf::from(format!("/dev/shm/paragc-proc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let space = PublicSpace::reserve().unwrap();
        let heap = space.create(&path, THREADS).unwrap();
        let anchor = heap.mutate(|m| m.root::<AnchorB>(m.alloc(Anchor { heads: Default::default() })));
        Self { path, heap, anchor }
    }

    /// 起一个子进程。
    fn spawn(&self, role: Role, chain: usize, rounds: u64) -> Kid {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child", "--nocapture", "--test-threads", "1", "-q"])
            .env(ROLE, role.name())
            .env(PATH, &self.path)
            .env(ANCHOR, self.anchor.index().to_string())
            .env(CHAIN, chain.to_string())
            .env(ROUNDS, rounds.to_string())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Kid { child, out }
    }

    /// 等 `f` 成立，最多 [`DEADLINE`]。
    fn eventually(&self, what: &str, f: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        while !f(self) {
            assert!(start.elapsed() < DEADLINE, "{what} 超时");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// 子进程与它的输出。
struct Kid {
    child: Child,
    out: BufReader<ChildStdout>,
}

impl Kid {
    /// 读输出直到某一行以 `prefix` 开头，返回这一行剩下的部分。
    fn line(&mut self, prefix: &str) -> String {
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(self.out.read_line(&mut line).unwrap(), 0, "子进程没等到 {prefix} 就退出了");
            if let Some(rest) = line.trim_end().strip_prefix(prefix) {
                return rest.to_owned();
            }
        }
    }

    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    /// 等它正常跑完，返回它打印的链长。
    fn finish(mut self) -> u64 {
        let len = self.line(DONE).parse().unwrap();
        assert!(self.child.wait().unwrap().success(), "子进程失败");
        len
    }
}

/// 子进程入口：没有 [`ROLE`] 时什么都不做（父进程跑整个文件时也会跑到这里）。
#[test]
fn child() {
    let Ok(role) = std::env::var(ROLE) else { return };
    let role: Role = role.parse().unwrap();
    let env = |k: &str| std::env::var(k).unwrap();
    let path = PathBuf::from(env(PATH));
    let (chain, rounds): (usize, u64) = (env(CHAIN).parse().unwrap(), env(ROUNDS).parse().unwrap());
    let space = PublicSpace::reserve().unwrap();
    let heap = space.open(Path::new(&path), THREADS).unwrap();
    let anchor: Public<AnchorB> = heap.mutate(|m| m.find(env(ANCHOR).parse().unwrap())).expect("凭入口号找到锚");
    match role {
        Role::Churn => {
            for r in 0..rounds {
                churn(&heap, &anchor, chain, PUSH, r % 50);
                check(&heap, &anchor, chain);
                if r.is_multiple_of(5) {
                    heap.collect();
                }
            }
            println!("{DONE}{}", length(&heap, &anchor, chain));
        }
        Role::ChurnForever => {
            println!("{READY}");
            let mut r: u64 = 0;
            loop {
                churn(&heap, &anchor, chain, PUSH, PUSH - 1 + r % 3);
                check(&heap, &anchor, chain);
                if r.is_multiple_of(4) {
                    heap.collect();
                }
                r += 1;
            }
        }
        Role::Hold => {
            // 一条只有本进程句柄挂着的链：进程死后入口还掉，这条链就该被回收
            let _mine = heap.mutate(|m| {
                let mut top = None;
                for value in 0..10_000 {
                    top = Some(m.alloc(Node { value, next: top.into() }));
                }
                m.root::<NodeB>(top.unwrap())
            });
            heap.mutate(|_| {
                println!("{READY}");
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
        }
        Role::CollectForever => {
            println!("{READY}");
            loop {
                heap.collect();
            }
        }
    }
}

#[test]
fn processes_share_one_heap() {
    const KIDS: usize = 4;
    const ROUNDS_EACH: u64 = 40;
    let p = Parent::new("share");
    let kids: Vec<Kid> = (1..=KIDS).map(|c| p.spawn(Role::Churn, c, ROUNDS_EACH)).collect();
    // 父进程同时改 0 号链、回收
    for r in 0..ROUNDS_EACH {
        churn(&p.heap, &p.anchor, 0, PUSH, r % 50);
        check(&p.heap, &p.anchor, 0);
        p.heap.collect();
    }
    let lens: Vec<u64> = kids.into_iter().map(Kid::finish).collect();
    p.heap.collect();
    let mut live = check(&p.heap, &p.anchor, 0);
    for (c, len) in (1..=KIDS).zip(lens) {
        assert_eq!(check(&p.heap, &p.anchor, c), len, "子进程改的链在父进程里看到的长度一样");
        live += len;
    }
    assert_eq!(p.heap.stats().live, ANCHOR_BYTES + NODE * usize::try_from(live).unwrap(), "存活字节数恰好是各条链");
    assert_eq!(p.heap.attached(), 1, "正常退出的子进程空出了挂接槽");
}

#[test]
fn killed_mutator_does_not_block_collection() {
    let p = Parent::new("hold");
    let mut kid = p.spawn(Role::Hold, 1, 0);
    kid.line(READY);
    assert_eq!(p.heap.attached(), 2);
    let before = p.heap.stats().collections;
    std::thread::scope(|s| {
        let collecting = s.spawn(|| p.heap.collect());
        // 子进程活着、停在 mutate 里：回收只能等
        std::thread::sleep(Duration::from_millis(300));
        assert!(!collecting.is_finished(), "子进程还在 mutate 里，回收不该开始");
        kid.kill();
        collecting.join().unwrap();
    });
    assert_eq!(p.heap.stats().collections, before + 1);
    assert_eq!(p.heap.attached(), 1, "死掉的子进程被收拾了");
    assert_eq!(p.heap.stats().live, ANCHOR_BYTES, "它那条只有自己挂着的链随入口还掉一起被回收");
}

#[test]
fn killed_collector_is_taken_over() {
    const TRIES: usize = 12;
    let p = Parent::new("collector");
    // 一条大一点的链，让每次回收要花几毫秒，杀的时候多半正在回收
    churn(&p.heap, &p.anchor, 0, 300_000, 0);
    let mut rng = Rng::new();
    for _ in 0..TRIES {
        let mut kid = p.spawn(Role::CollectForever, 1, 0);
        kid.line(READY);
        std::thread::sleep(Duration::from_millis(5 + rng.below(30)));
        kid.kill();
        // 父进程照常用堆：回收方死了的话，mutate 或回收会接手
        assert_eq!(check(&p.heap, &p.anchor, 0), 300_000);
        p.heap.collect();
        p.eventually("死掉的回收方被收拾", |p| p.heap.attached() == 1);
    }
    assert!(p.heap.stats().takeovers > 0, "{TRIES} 次里至少有一次杀在回收中途、被接手");
    assert_eq!(p.heap.stats().live, ANCHOR_BYTES + NODE * 300_000);
}

#[test]
fn random_kills_while_processes_churn() {
    const KIDS: usize = 4;
    const KILLS: usize = 30;
    let p = Parent::new("kills");
    let mut rng = Rng::new();
    let start = |c: usize| {
        let mut k = p.spawn(Role::ChurnForever, c, 0);
        k.line(READY);
        Some(k)
    };
    let mut kids: Vec<Option<Kid>> = (1..=KIDS).map(start).collect();
    for i in 0..KILLS {
        churn(&p.heap, &p.anchor, 0, PUSH, PUSH - 1 + i as u64 % 3);
        check(&p.heap, &p.anchor, 0);
        std::thread::sleep(Duration::from_millis(rng.below(20)));
        // 随便杀一个，再起一个新的接着改同一条链（先杀后起：同一条链只能有一个进程在改）
        let c = usize::try_from(rng.below(KIDS as u64)).unwrap();
        kids[c].take().expect("每个位置都有一个子进程").kill();
        kids[c] = start(c + 1);
        if i.is_multiple_of(3) {
            p.heap.collect();
        }
    }
    for k in kids.into_iter().flatten() {
        k.kill();
    }
    p.heap.collect();
    p.eventually("死掉的子进程都被收拾", |p| {
        p.heap.collect();
        p.heap.attached() == 1
    });
    let live: u64 = (0..=KIDS).map(|c| check(&p.heap, &p.anchor, c)).sum();
    assert_eq!(p.heap.stats().live, ANCHOR_BYTES + NODE * usize::try_from(live).unwrap(), "被杀进程的东西都收干净了，挂着的一个不少");
}
