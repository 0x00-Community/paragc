//! 多个进程挂同一个公开堆。子进程是本测试程序自己：带上 [`ROLE`] 等环境变量、只跑 `child` 那一条。
//! 逐条跑：`cargo test --features crossheap --test processes -- --exact <名字>`
#![cfg(all(target_os = "linux", feature = "crossheap", not(miri)))]

use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use paragc::{Heap, HeapDead, CrossHeapRoot, CrossHeap, CrossHeapRef, CrossHeapSlot, CrossHeapSpace, Root, Shared, Trace, brand, cross_heap_brand};

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
/// 建堆的子进程打印这个前缀 + 链头的入口号 + 堆号
const CREATED: &str = "PARAGC-CREATED ";
/// 建堆的子进程：调 persist（堆与链头都留着，主人不在了也不死）
const PERSIST: &str = "PARAGC_PERSIST";
/// 建堆的子进程 [`ROUNDS`] 取这个值：建好后一直等着被杀（否则正常退出）
const STAY: u64 = 1;
/// 建堆的子进程挂的链长
const CREATED_LEN: u64 = 5_000;
/// 每轮在链头压入的节点数
const PUSH: u64 = 200;
/// 杀掉的进程最迟这么久内要被收拾、回收要能做完
const DEADLINE: Duration = Duration::from_secs(10);

#[repr(C)]
#[derive(Shared)]
struct Node<'p> {
    value: u64,
    next: CrossHeapSlot<'p, Node<'p>>,
}

cross_heap_brand!(NodeB = for<'p> Node<'p>);

/// 每条链一个头；0 号归父进程，其余给子进程
#[repr(C)]
#[derive(Shared)]
struct Anchor<'p> {
    heads: [CrossHeapSlot<'p, Node<'p>>; 8],
}

cross_heap_brand!(AnchorB = for<'p> Anchor<'p>);

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
    /// 在 [`PATH`] 建一个堆、挂一条 [`CREATED_LEN`] 节点的链，打印入口号与堆号；然后 persist 后退出，或一直等着被杀
    Create,
    /// 八进程场景里的一员：一个公开堆 + 一个私有堆，引用别人的公开堆；按标准输入的命令做事（见 [`Pair`]）
    Pair,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Churn => "churn",
            Self::ChurnForever => "churn-forever",
            Self::Hold => "hold",
            Self::CollectForever => "collect-forever",
            Self::Create => "create",
            Self::Pair => "pair",
        }
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        [Self::Churn, Self::ChurnForever, Self::Hold, Self::CollectForever, Self::Create, Self::Pair].into_iter().find(|r| r.name() == s).ok_or_else(|| format!("未知的子进程角色 {s}"))
    }
}

/// 链现在的长度：链上的值从头起是 len-1, len-2, …, 0（任何一步被杀都保持这个形状），头的值 + 1 就是长度。
fn length(heap: &CrossHeap, anchor: &CrossHeapRoot<AnchorB>, chain: usize) -> u64 {
    heap.mutate(|m| anchor.get(m).heads[chain].get(m).map_or(0, |n| n.value + 1))
}

/// 在链头压入 `push` 个、再弹掉 `pop` 个；返回新长度。
fn churn(heap: &CrossHeap, anchor: &CrossHeapRoot<AnchorB>, chain: usize, push: u64, pop: u64) -> u64 {
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
fn check(heap: &CrossHeap, anchor: &CrossHeapRoot<AnchorB>, chain: usize) -> u64 {
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
    heap: CrossHeap,
    anchor: CrossHeapRoot<AnchorB>,
}

impl Parent {
    fn new(name: &str) -> Self {
        let path = PathBuf::from(format!("/dev/shm/paragc-proc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let space = CrossHeapSpace::reserve().unwrap();
        let heap = space.create(&path, THREADS).unwrap();
        let anchor = heap.mutate(|m| m.root::<AnchorB>(m.alloc(Anchor { heads: Default::default() })));
        Self { path, heap, anchor }
    }

    /// 起一个子进程。
    fn spawn(&self, role: Role, chain: usize, rounds: u64) -> Kid {
        Kid::spawn(role, &self.path, self.anchor.index(), chain, rounds, false)
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
    input: ChildStdin,
}

impl Kid {
    fn spawn(role: Role, path: &Path, anchor: u32, chain: usize, rounds: u64, persist: bool) -> Self {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        if persist {
            cmd.env(PERSIST, "1");
        }
        let mut child = cmd
            .args(["--exact", "child", "--nocapture", "--test-threads", "1", "-q"])
            .env(ROLE, role.name())
            .env(PATH, path)
            .env(ANCHOR, anchor.to_string())
            .env(CHAIN, chain.to_string())
            .env(ROUNDS, rounds.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let input = child.stdin.take().unwrap();
        Self { child, out, input }
    }

    /// 给子进程发一行命令。
    fn send(&mut self, line: &str) {
        writeln!(self.input, "{line}").unwrap();
        self.input.flush().unwrap();
    }

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
    let space = CrossHeapSpace::reserve().unwrap();
    if let Role::Pair = role {
        Pair::run(&space, &path, chain);
        return;
    }
    if let Role::Create = role {
        let heap = space.create(&path, THREADS).unwrap();
        let chain = heap.mutate(|m| {
            let mut top = None;
            for value in 0..CREATED_LEN {
                top = Some(m.alloc(Node { value, next: top.into() }));
            }
            m.root::<NodeB>(top.unwrap())
        });
        let index = if std::env::var(PERSIST).is_ok() {
            // 链头变成不归任何进程的根，堆也留着：主人不在了别人照样接着用
            heap.persist();
            chain.persist()
        } else {
            // 没有 persist：堆跟着本进程死（正常退出时句柄析构删文件；被杀时别人发现后删）
            chain.index()
        };
        println!("{CREATED}{index} {}", heap.id());
        if rounds != STAY {
            return;
        }
        println!("{READY}");
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let heap = space.open(Path::new(&path), THREADS).unwrap();
    let anchor: CrossHeapRoot<AnchorB> = heap.mutate(|m| m.find(env(ANCHOR).parse().unwrap())).expect("凭入口号找到锚");
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
        Role::Create | Role::Pair => unreachable!("上面已经处理"),
    }
}

/// 一个单独的目录（里面的堆号标记只归这条测试）。
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(format!("/dev/shm/paragc-dir-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 读建堆子进程打印的（链头入口号, 堆号）。
fn created(kid: &mut Kid) -> (u32, usize) {
    let line = kid.line(CREATED);
    let mut parts = line.split_whitespace().map(|p| p.parse::<u64>().unwrap());
    (u32::try_from(parts.next().unwrap()).unwrap(), usize::try_from(parts.next().unwrap()).unwrap())
}

/// 打开 `path` 的堆，凭入口号找到链、核对 0..CREATED_LEN，回收后核对存活字节数与挂接数。
fn reopen_and_check(path: &Path, index: u32) -> CrossHeap {
    let space = CrossHeapSpace::reserve().unwrap();
    let heap = space.open(path, THREADS).unwrap();
    let chain: CrossHeapRoot<NodeB> = heap.mutate(|m| m.find(index)).expect("建堆的进程挂的链还在");
    let sum = heap.mutate(|m| {
        let (mut s, mut cur) = (0, Some(chain.get(m)));
        while let Some(n) = cur {
            s += n.value;
            cur = n.next.get(m);
        }
        s
    });
    assert_eq!(sum, CREATED_LEN * (CREATED_LEN - 1) / 2);
    heap.collect();
    assert_eq!(heap.attached(), 1, "建堆的进程已经不在，它的挂接被收拾");
    assert_eq!(heap.stats().live, NODE * usize::try_from(CREATED_LEN).unwrap(), "链还活着（我们的句柄）");
    drop(chain);
    heap
}

#[test]
fn restart_after_the_creator_is_gone() {
    let dir = Dir::new("restart");
    // 没有 persist、被 kill -9：堆跟着死。别人打开时发现主人不在了，打开失败，文件与标记删掉
    let crashed = dir.file("crashed");
    let mut kid = Kid::spawn(Role::Create, &crashed, 0, 0, STAY, false);
    let (_, number) = created(&mut kid);
    kid.line(READY);
    kid.kill();
    let space = CrossHeapSpace::reserve().unwrap();
    let err = space.open(&crashed, THREADS).err().expect("主人死了，堆跟着死");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(!crashed.exists() && !dir.file(&format!(".paragc-{number}")).exists(), "死堆的文件与标记删掉了");
    // 没有 persist、正常退出：句柄析构时删掉
    let exited = dir.file("exited");
    let mut kid = Kid::spawn(Role::Create, &exited, 0, 0, 0, false);
    let (_, number) = created(&mut kid);
    assert!(kid.child.wait().unwrap().success());
    assert!(!exited.exists() && !dir.file(&format!(".paragc-{number}")).exists());
    // persist 之后被 kill -9：堆留着，别的进程打开接着用；它持有的入口在回收时还掉
    let killed = dir.file("killed");
    let mut kid = Kid::spawn(Role::Create, &killed, 0, 0, STAY, true);
    let (index, _) = created(&mut kid);
    kid.line(READY);
    kid.kill();
    let heap = reopen_and_check(&killed, index);
    assert!(heap.is_alive(), "persist 过的堆主人不在了也活着");
    drop(heap);
    // persist 之后正常退出：文件与堆号标记都留着
    let kept = dir.file("kept");
    let mut kid = Kid::spawn(Role::Create, &kept, 0, 0, 0, true);
    let (index, number) = created(&mut kid);
    assert!(kid.child.wait().unwrap().success());
    assert!(kept.exists() && dir.file(&format!(".paragc-{number}")).exists());
    let heap = reopen_and_check(&kept, index);
    // 不归任何进程的根要手动还；还掉以后链就收掉
    assert!(heap.mutate(|m| m.release(index)));
    assert!(!heap.mutate(|m| m.release(index)), "还过一次就空了");
    heap.collect();
    assert_eq!(heap.stats().live, 0);
}

#[test]
fn heap_numbers_do_not_collide_across_processes() {
    let dir = Dir::new("numbers");
    let first = dir.file("first");
    let mut kid = Kid::spawn(Role::Create, &first, 0, 0, 0, true);
    let (_, taken) = created(&mut kid);
    assert!(kid.child.wait().unwrap().success());
    // 本进程在同一个目录建堆：堆号不能和别的进程建的撞，两个堆在本进程能同时打开
    let space = CrossHeapSpace::reserve().unwrap();
    let second = space.create(dir.file("second"), THREADS).unwrap();
    assert_ne!(second.id(), taken);
    let reopened = space.open(&first, THREADS).unwrap();
    assert_eq!(reopened.id(), taken);
    // 堆文件没了，它的标记就是过期的：号收回重用
    drop(reopened);
    std::fs::remove_file(&first).unwrap();
    let third = space.create(dir.file("third"), THREADS).unwrap();
    assert_eq!(third.id(), taken, "过期标记的堆号收回重用");
    drop((second, third));
    let left: Vec<_> = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "建堆的句柄析构后文件与标记都删了：{left:?}");
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

/// 八进程场景：进程数、每个公开堆 / 私有堆里链的长度
const PAIRS: usize = 8;
const PAIR_NODES: u64 = 500_000;
/// 八进程场景的命令与回报（标准输入 / 输出的行首）
const PAIR_INFO: &str = "PAIR ";
const PEERS: &str = "PEERS ";
const LINKED: &str = "PAIR-LINKED";
const RELEASE: &str = "RELEASE";
const CHECK: &str = "CHECK ";
const CHECKED: &str = "PAIR-OK ";
const EXIT: &str = "EXIT";

/// 公开堆里的「集线器」：指向每个别的进程的公开堆里给本进程的礼物
#[repr(C)]
#[derive(Shared)]
struct Hub<'p> {
    refs: [CrossHeapRef<'p, NodeB>; PAIRS],
}

cross_heap_brand!(HubB = for<'p> Hub<'p>);

const HUB: usize = (HEADER + size_of::<Hub<'static>>()).next_multiple_of(8);

/// 私有堆里的卡片：拿着别的进程公开堆里那条链的句柄
#[derive(Trace)]
struct Card {
    peer: usize,
    chain: CrossHeapRoot<NodeB>,
}

brand!(CardB = for<'gc> Card);

/// 私有堆里的链
#[derive(Trace)]
struct Bead<'gc> {
    value: u64,
    next: Option<paragc::Gc<'gc, Bead<'gc>>>,
}

brand!(BeadB = for<'gc> Bead<'gc>);

/// 礼物的值：给谁的、谁给的
fn gift_value(from: usize, to: usize) -> u64 {
    1_000_000 + (from * PAIRS + to) as u64
}

/// 八进程场景里的一个进程。
struct Pair {
    me: usize,
    mine: CrossHeap,
    chain: CrossHeapRoot<NodeB>,
    hub: CrossHeapRoot<HubB>,
    private: Heap,
    beads: Root<BeadB>,
    /// 别的进程的公开堆（死了的放掉）
    peers: Vec<Option<CrossHeap>>,
    cards: Vec<Option<Root<CardB>>>,
}

impl Pair {
    /// 子进程这一侧：建堆、报信息、等 PEERS、连上别人、等 RELEASE、报 READY，之后边干活边听命令。
    fn run(space: &CrossHeapSpace, path: &Path, me: usize) {
        let mine = space.create(path, THREADS).unwrap();
        let chain = mine.mutate(|m| {
            let mut top = None;
            for value in 0..PAIR_NODES {
                top = Some(m.alloc(Node { value, next: top.into() }));
            }
            m.root::<NodeB>(top.unwrap())
        });
        let hub = mine.mutate(|m| m.root::<HubB>(m.alloc(Hub { refs: Default::default() })));
        // 给每个别的进程一份礼物：等它的集线器指上来、本进程放掉句柄后，只靠它的引用活着
        let gifts: Vec<Option<CrossHeapRoot<NodeB>>> = (0..PAIRS).map(|j| (j != me).then(|| mine.mutate(|m| m.root(m.alloc(Node { value: gift_value(me, j), next: CrossHeapSlot::default() }))))).collect();
        let gift_list: Vec<String> = gifts.iter().map(|g| g.as_ref().map_or(u32::MAX, CrossHeapRoot::index).to_string()).collect();
        println!("{PAIR_INFO}{me} {} {}", chain.index(), gift_list.join(","));
        let mut stdin = std::io::stdin().lines();
        let peers_line = stdin.next().unwrap().unwrap();
        let peers_line = peers_line.strip_prefix(PEERS).expect("PEERS");
        // 每个进程：路径 链头入口号 给各进程的礼物入口号（逗号分隔）；用 ; 分开
        let infos: Vec<(PathBuf, u32, Vec<u32>)> = peers_line
            .split(';')
            .map(|p| {
                let mut w = p.split(' ');
                let path = PathBuf::from(w.next().unwrap());
                let chain = w.next().unwrap().parse().unwrap();
                let gifts = w.next().unwrap().split(',').map(|g| g.parse().unwrap()).collect();
                (path, chain, gifts)
            })
            .collect();
        let peers: Vec<Option<CrossHeap>> = infos.iter().enumerate().map(|(j, (p, _, _))| (j != me).then(|| space.open(p, THREADS).unwrap())).collect();
        let mut private = Heap::new(THREADS);
        let beads = private.mutate(|m| {
            let mut top = None;
            for value in 0..PAIR_NODES {
                top = Some(m.alloc(Bead { value, next: top }));
            }
            m.root(top.unwrap())
        });
        let mut cards = Vec::new();
        for (j, peer) in peers.iter().enumerate() {
            cards.push(peer.as_ref().map(|h| {
                let chain: CrossHeapRoot<NodeB> = h.mutate(|m| m.find(infos[j].1)).expect("别人的链");
                private.mutate(|m| m.root(m.alloc(Card { peer: j, chain })))
            }));
            // 集线器第 j 格指向 j 给本进程的礼物
            if let Some(h) = peer {
                mine.mutate_with(h, |mi, mj| {
                    let gift: CrossHeapRoot<NodeB> = mj.find(infos[j].2[me]).expect("礼物");
                    hub.get(mi).refs[j].set(mi, mj, Some(gift.get(mj)));
                });
            }
        }
        println!("{LINKED}");
        assert_eq!(stdin.next().unwrap().unwrap(), RELEASE);
        drop(gifts);
        private.collect();
        let mut pair = Self { me, mine, chain, hub, private, beads, peers, cards };
        println!("{READY}");
        pair.serve(&mut stdin);
    }

    /// 后台线程不停地分配、回收、读别人；主线程等命令。
    fn serve(&mut self, stdin: &mut std::io::Lines<std::io::StdinLock<'static>>) {
        loop {
            let stop = std::sync::atomic::AtomicBool::new(false);
            let line = std::thread::scope(|s| {
                let this = &*self;
                s.spawn(|| this.churn(&stop));
                let line = stdin.next().and_then(Result::ok).unwrap_or_else(|| EXIT.to_owned());
                stop.store(true, std::sync::atomic::Ordering::Release);
                line
            });
            if line == EXIT {
                return;
            }
            let dead: Vec<usize> = line.strip_prefix(CHECK).expect("CHECK").split(',').filter(|w| !w.is_empty()).map(|w| w.parse().unwrap()).collect();
            let bytes = self.check(&dead);
            println!("{CHECKED}{} {bytes}", self.me);
        }
    }

    /// 后台：在自己的公开堆上分配垃圾、回收，读活着的别人（读不到就跳过）。
    fn churn(&self, stop: &std::sync::atomic::AtomicBool) {
        let mut round = 0u64;
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            self.mine.mutate(|m| {
                for value in 0..1_000 {
                    m.alloc(Node { value, next: CrossHeapSlot::default() });
                }
            });
            for (j, peer) in self.peers.iter().enumerate() {
                // 同时进两个公开堆用 mutate_with（按堆号顺序进门）；死了的不进
                let Some(h) = peer.as_ref().filter(|h| h.is_alive()) else { continue };
                self.mine.mutate_with(h, |mi, mj| self.hub.get(mi).refs[j].get(mj).map(|g| g.value));
            }
            if round.is_multiple_of(4) {
                self.mine.collect();
            }
            round += 1;
        }
    }

    /// 核对（后台已停）：死了的别人读不了、放掉；活着的读得到、值对；自己的堆存活量恰好是「链 + 集线器 + 活着的人手里的礼物」。返回自己公开堆的存活字节数。
    fn check(&mut self, dead: &[usize]) -> usize {
        let me = self.me;
        for j in (0..PAIRS).filter(|&j| j != me) {
            let h = self.peers[j].as_ref();
            if dead.contains(&j) {
                if let Some(h) = h {
                    assert!(!h.is_alive(), "进程 {j} 死了，它的公开堆也该死");
                    assert_eq!(h.try_mutate(|_| ()), Err(HeapDead));
                }
                // 放掉指向它的句柄：私有堆里的卡片、本进程对它的映射
                self.cards[j] = None;
                self.peers[j] = None;
                continue;
            }
            let h = h.expect("活着的都还开着");
            assert!(h.is_alive(), "进程 {j} 还活着");
            let gift = self.mine.mutate_with(h, |mi, mj| self.hub.get(mi).refs[j].get(mj).map(|g| g.value));
            assert_eq!(gift, Some(gift_value(j, me)), "集线器指着 {j} 给的礼物");
            let card = self.cards[j].as_ref().expect("活着的卡片还在");
            let head = self.private.mutate(|m| h.try_mutate(|mj| m.get(card).chain.get(mj).value)).expect("活着的读得到");
            assert_eq!(head, PAIR_NODES - 1);
            assert_eq!(self.private.mutate(|m| m.get(card).peer), j);
        }
        // 私有堆：卡片析构（句柄还掉入口），珠链完好
        self.private.collect();
        let sum = self.private.mutate(|m| {
            let (mut s, mut cur) = (0, Some(m.get(&self.beads)));
            while let Some(b) = cur {
                s += b.value;
                cur = b.next;
            }
            s
        });
        assert_eq!(sum, PAIR_NODES * (PAIR_NODES - 1) / 2);
        // 自己的公开堆：死了的人持有的礼物登记还掉，礼物收掉
        self.mine.collect();
        let alive_peers = (0..PAIRS).filter(|&j| j != me && !dead.contains(&j)).count();
        let expected = NODE * usize::try_from(PAIR_NODES).unwrap() + HUB + NODE * alive_peers;
        let live = self.mine.stats().live;
        assert_eq!(live, expected, "进程 {me}：存活 = 链 + 集线器 + {alive_peers} 份礼物");
        // 和活着的别人一起回收也照常
        let mut group = vec![&self.mine];
        group.extend(self.peers.iter().flatten());
        CrossHeap::collect_group(&group);
        assert_eq!(self.mine.mutate(|m| self.chain.get(m).value), PAIR_NODES - 1);
        live
    }
}

/// 进程的内存峰值与 /dev/shm 用量（在 cgroup 里跑时有意义）。
fn report_memory(what: &str) {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let dir = cgroup.trim().rsplit(':').next().unwrap_or_default();
    let read = |f: &str| std::fs::read_to_string(format!("/sys/fs/cgroup{dir}/{f}")).map_or_else(|_| "?".into(), |v| v.trim().to_owned());
    let shm = std::fs::read_to_string("/proc/meminfo").unwrap_or_default().lines().find(|l| l.starts_with("Shmem:")).unwrap_or_default().to_owned();
    eprintln!("[内存] {what}：cgroup memory.max={} memory.peak={} memory.current={}；{shm}", read("memory.max"), read("memory.peak"), read("memory.current"));
}

#[test]
fn eight_processes_half_die() {
    let dir = Dir::new("pairs");
    let mut kids: Vec<Option<Kid>> = (0..PAIRS).map(|i| Some(Kid::spawn(Role::Pair, &dir.file(&format!("heap-{i}")), 0, i, 0, false))).collect();
    let infos: Vec<String> = kids
        .iter_mut()
        .enumerate()
        .map(|(i, k)| {
            let line = k.as_mut().unwrap().line(PAIR_INFO);
            let mut w = line.split(' ');
            assert_eq!(w.next().unwrap().parse::<usize>().unwrap(), i);
            format!("{} {} {}", dir.file(&format!("heap-{i}")).display(), w.next().unwrap(), w.next().unwrap())
        })
        .collect();
    let peers = format!("{PEERS}{}", infos.join(";"));
    for k in kids.iter_mut().flatten() {
        k.send(&peers);
    }
    for k in kids.iter_mut().flatten() {
        k.line(LINKED);
    }
    for k in kids.iter_mut().flatten() {
        k.send(RELEASE);
    }
    for k in kids.iter_mut().flatten() {
        k.line(READY);
    }
    report_memory("八个进程都起来了");
    // 先让后台跑一会儿，再全体核对一次（没人死）
    std::thread::sleep(Duration::from_millis(200));
    for k in kids.iter_mut().flatten() {
        k.send(CHECK);
    }
    for k in kids.iter_mut().flatten() {
        k.line(CHECKED);
    }
    // 后台还在跑的时候杀掉一半
    std::thread::sleep(Duration::from_millis(200));
    let victims = [1, 3, 4, 6];
    for &v in &victims {
        kids[v].take().unwrap().kill();
    }
    std::thread::sleep(Duration::from_millis(200));
    let list: Vec<String> = victims.iter().map(ToString::to_string).collect();
    let check = format!("{CHECK}{}", list.join(","));
    for k in kids.iter_mut().flatten() {
        k.send(&check);
    }
    for k in kids.iter_mut().flatten() {
        k.line(CHECKED);
    }
    report_memory("死掉四个之后");
    // 死了的堆：文件与标记都删了；活着的还在
    for i in 0..PAIRS {
        let alive = !victims.contains(&i);
        assert_eq!(dir.file(&format!("heap-{i}")).exists(), alive, "堆 {i} 的文件");
    }
    let markers = std::fs::read_dir(&dir.0).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with(".paragc-")).count();
    assert_eq!(markers, PAIRS - victims.len(), "死堆的标记都删了");
    for k in kids.iter_mut().flatten() {
        k.send(EXIT);
    }
    for k in kids.into_iter().flatten() {
        assert!(k.child.wait_with_output().unwrap().status.success());
    }
    // 活着的也都退出了：它们的堆跟着死，目录空了
    let left: Vec<_> = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "进程都走了，堆都跟着死：{left:?}");
}

#[test]
fn simultaneous_creates_get_distinct_numbers() {
    // 同一个目录里同时建堆：领号的标记、堆文件、超级块是几步写的，别人读到中间状态时不能当成过期标记抢走
    const ROUNDS_EACH: usize = 5;
    for round in 0..ROUNDS_EACH {
        let dir = Dir::new(&format!("race-{round}"));
        let mut kids: Vec<Kid> = (0..PAIRS).map(|i| Kid::spawn(Role::Create, &dir.file(&format!("h{i}")), 0, 0, STAY, false)).collect();
        let mut numbers: Vec<usize> = kids.iter_mut().map(|k| created(k).1).collect();
        for k in &mut kids {
            k.line(READY);
        }
        numbers.sort_unstable();
        let unique = numbers.windows(2).all(|w| w[0] != w[1]);
        for k in kids {
            k.kill();
        }
        assert!(unique, "第 {round} 轮同时建的堆号撞了：{numbers:?}");
    }
}
