//! Several processes attached to the same public heap. The child processes are this test program itself: started with [`ROLE`] and other environment variables, running only the `child` test.
//! Run one at a time: `cargo test --features crossheap --test processes -- --exact <name>`
#![cfg(all(target_os = "linux", feature = "crossheap", not(miri)))]

use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use paragc::{Heap, HeapDead, CrossHeapRoot, CrossHeap, CrossHeapRef, CrossHeapSlot, CrossHeapSpace, Root, Shared, Trace, brand, cross_heap_brand};

const THREADS: NonZeroUsize = NonZeroUsize::new(4).unwrap();
/// Public heap object header
const HEADER: usize = 16;

/// Environment variables of a child: what to do, the heap file, the anchor object's entry number, which chain to use, how many rounds
const ROLE: &str = "PARAGC_CHILD";
const PATH: &str = "PARAGC_PATH";
const ANCHOR: &str = "PARAGC_ANCHOR";
const CHAIN: &str = "PARAGC_CHAIN";
const ROUNDS: &str = "PARAGC_ROUNDS";
/// A child prints this line when it is ready
const READY: &str = "PARAGC-READY";
/// A child prints this prefix + the chain length when done
const DONE: &str = "PARAGC-LEN ";
/// The heap-creating child prints this prefix + the chain head's entry number + the heap number
const CREATED: &str = "PARAGC-CREATED ";
/// The heap-creating child calls persist (the heap and the chain head stay, so it does not die with its owner)
const PERSIST: &str = "PARAGC_PERSIST";
/// The heap-creating child with [`ROUNDS`] set to this waits to be killed after creating (otherwise it exits normally)
const STAY: u64 = 1;
/// Length of the chain hung by the heap-creating child
const CREATED_LEN: u64 = 5_000;
/// Nodes pushed at the chain head per round
const PUSH: u64 = 200;
/// A killed process must be cleaned up, and a collection be able to finish, within this long
const DEADLINE: Duration = Duration::from_secs(10);

#[repr(C)]
#[derive(Shared)]
struct Node<'p> {
    value: u64,
    next: CrossHeapSlot<'p, Node<'p>>,
}

cross_heap_brand!(NodeB = for<'p> Node<'p>);

/// One head per chain; number 0 belongs to the parent, the rest to children
#[repr(C)]
#[derive(Shared)]
struct Anchor<'p> {
    heads: [CrossHeapSlot<'p, Node<'p>>; 8],
}

cross_heap_brand!(AnchorB = for<'p> Anchor<'p>);

const NODE: usize = (HEADER + size_of::<Node<'static>>()).next_multiple_of(8);
const ANCHOR_BYTES: usize = (HEADER + size_of::<Anchor<'static>>()).next_multiple_of(8);

/// What a child does.
#[derive(Clone, Copy)]
enum Role {
    /// Push and pop on its own chain for [`ROUNDS`] rounds (collecting every 5), print the chain length and exit
    Churn,
    /// Keep pushing, popping and collecting until killed
    ChurnForever,
    /// Make a chain held only by itself, then stay inside mutate until killed
    Hold,
    /// Keep collecting until killed
    CollectForever,
    /// Create a heap at [`PATH`], hang a chain of [`CREATED_LEN`] nodes and print the entry number and heap number; then persist and exit, or wait to be killed
    Create,
    /// A member of the eight-process scenario: one public heap + one private heap, referencing the others' public heaps; acts on commands from standard input (see [`Pair`])
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
        [Self::Churn, Self::ChurnForever, Self::Hold, Self::CollectForever, Self::Create, Self::Pair].into_iter().find(|r| r.name() == s).ok_or_else(|| format!("unknown child role {s}"))
    }
}

/// The chain's current length: its values from the head are len-1, len-2, …, 0 (this shape holds whichever step is killed), so the head's value + 1 is the length.
fn length(heap: &CrossHeap, anchor: &CrossHeapRoot<AnchorB>, chain: usize) -> u64 {
    heap.mutate(|m| anchor.get(m).heads[chain].get(m).map_or(0, |n| n.value + 1))
}

/// Push `push` nodes at the chain head, then pop `pop`; returns the new length.
fn churn(heap: &CrossHeap, anchor: &CrossHeapRoot<AnchorB>, chain: usize, push: u64, pop: u64) -> u64 {
    heap.mutate(|m| {
        let top = &anchor.get(m).heads[chain];
        let len = top.get(m).map_or(0, |n| n.value + 1);
        for v in len..len + push {
            let n = m.alloc(Node { value: v, next: top.get(m).into() });
            top.set(m, Some(n));
        }
        for _ in 0..pop.min(len + push) {
            let first = top.get(m).expect("the chain is not shorter than what is popped");
            top.set(m, first.next.get(m));
        }
        top.get(m).map_or(0, |n| n.value + 1)
    })
}

/// The chain has the right shape: len-1, …, 0 from the head. Returns the length.
fn check(heap: &CrossHeap, anchor: &CrossHeapRoot<AnchorB>, chain: usize) -> u64 {
    heap.mutate(|m| {
        let mut cur = anchor.get(m).heads[chain].get(m);
        let len = cur.map_or(0, |n| n.value + 1);
        let mut expect = len;
        while let Some(n) = cur {
            expect -= 1;
            assert_eq!(n.value, expect, "values out of place on chain {chain}: an object was collected early or a pointer is corrupted");
            cur = n.next.get(m);
        }
        assert_eq!(expect, 0, "chain {chain} is broken");
        len
    })
}

/// A simple linear congruential generator (seeded with the time; printed on failure so it can be reproduced)
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
        let seed = u64::from(now.subsec_nanos()) ^ now.as_secs() ^ u64::from(std::process::id()) << 32;
        eprintln!("random seed {seed}");
        Self(seed | 1)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// The parent's side: the heap file, region, heap and anchor.
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

    /// Start a child.
    fn spawn(&self, role: Role, chain: usize, rounds: u64) -> Kid {
        Kid::spawn(role, &self.path, self.anchor.index(), chain, rounds, false)
    }

    /// Wait until `f` holds, at most [`DEADLINE`].
    fn eventually(&self, what: &str, f: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        while !f(self) {
            assert!(start.elapsed() < DEADLINE, "{what} timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// A child and its output.
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

    /// Send the child a line of command.
    fn send(&mut self, line: &str) {
        writeln!(self.input, "{line}").unwrap();
        self.input.flush().unwrap();
    }

    /// Read output until a line starts with `prefix`, returning the rest of that line.
    fn line(&mut self, prefix: &str) -> String {
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(self.out.read_line(&mut line).unwrap(), 0, "the child exited before {prefix}");
            if let Some(rest) = line.trim_end().strip_prefix(prefix) {
                return rest.to_owned();
            }
        }
    }

    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    /// Wait for it to finish normally and return the chain length it printed.
    fn finish(mut self) -> u64 {
        let len = self.line(DONE).parse().unwrap();
        assert!(self.child.wait().unwrap().success(), "the child failed");
        len
    }
}

/// Child entry point: does nothing without [`ROLE`] (the parent also reaches it when running the whole file).
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
            // The chain head becomes a root owned by no process and the heap stays: others can continue after the owner is gone
            heap.persist();
            chain.persist()
        } else {
            // No persist: the heap dies with this process (on a normal exit dropping the handle deletes the file; when killed, whoever notices deletes it)
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
    let anchor: CrossHeapRoot<AnchorB> = heap.mutate(|m| m.find(env(ANCHOR).parse().unwrap())).expect("the anchor is found by its entry number");
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
            // A chain held only by this process's handle: once the process dies its entry is returned and the chain should be collected
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
        Role::Create | Role::Pair => unreachable!("handled above"),
    }
}

/// A directory of its own (the heap-number markers in it belong to this test only).
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

/// Read (chain head entry number, heap number) printed by the heap-creating child.
fn created(kid: &mut Kid) -> (u32, usize) {
    let line = kid.line(CREATED);
    let mut parts = line.split_whitespace().map(|p| p.parse::<u64>().unwrap());
    (u32::try_from(parts.next().unwrap()).unwrap(), usize::try_from(parts.next().unwrap()).unwrap())
}

/// Open the heap at `path`, find the chain by entry number and check 0..CREATED_LEN, then after collecting check live bytes and the attachment count.
fn reopen_and_check(path: &Path, index: u32) -> CrossHeap {
    let space = CrossHeapSpace::reserve().unwrap();
    let heap = space.open(path, THREADS).unwrap();
    let chain: CrossHeapRoot<NodeB> = heap.mutate(|m| m.find(index)).expect("the chain hung by the creating process is still there");
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
    assert_eq!(heap.attached(), 1, "the creating process is gone and its attachment was cleaned up");
    assert_eq!(heap.stats().live, NODE * usize::try_from(CREATED_LEN).unwrap(), "the chain is still alive (our handle)");
    drop(chain);
    heap
}

#[test]
fn restart_after_the_creator_is_gone() {
    let dir = Dir::new("restart");
    // No persist and killed with kill -9: the heap dies with it. Whoever opens it finds the owner gone, opening fails, and the file and marker are deleted
    let crashed = dir.file("crashed");
    let mut kid = Kid::spawn(Role::Create, &crashed, 0, 0, STAY, false);
    let (_, number) = created(&mut kid);
    kid.line(READY);
    kid.kill();
    let space = CrossHeapSpace::reserve().unwrap();
    let err = space.open(&crashed, THREADS).err().expect("the owner died, so the heap died with it");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(!crashed.exists() && !dir.file(&format!(".paragc-{number}")).exists(), "the dead heap's file and marker were deleted");
    // No persist and a normal exit: deleted when the handle is dropped
    let exited = dir.file("exited");
    let mut kid = Kid::spawn(Role::Create, &exited, 0, 0, 0, false);
    let (_, number) = created(&mut kid);
    assert!(kid.child.wait().unwrap().success());
    assert!(!exited.exists() && !dir.file(&format!(".paragc-{number}")).exists());
    // Killed with kill -9 after persist: the heap stays and other processes open it and continue; the entries it held are returned at collection
    let killed = dir.file("killed");
    let mut kid = Kid::spawn(Role::Create, &killed, 0, 0, STAY, true);
    let (index, _) = created(&mut kid);
    kid.line(READY);
    kid.kill();
    let heap = reopen_and_check(&killed, index);
    assert!(heap.is_alive(), "a persisted heap stays alive after its owner is gone");
    drop(heap);
    // A normal exit after persist: the file and heap-number marker both stay
    let kept = dir.file("kept");
    let mut kid = Kid::spawn(Role::Create, &kept, 0, 0, 0, true);
    let (index, number) = created(&mut kid);
    assert!(kid.child.wait().unwrap().success());
    assert!(kept.exists() && dir.file(&format!(".paragc-{number}")).exists());
    let heap = reopen_and_check(&kept, index);
    // A root owned by no process must be returned by hand; once returned the chain is collected
    assert!(heap.mutate(|m| m.release(index)));
    assert!(!heap.mutate(|m| m.release(index)), "empty after returning it once");
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
    // This process creates heaps in the same directory: heap numbers must not collide with other processes', and the two heaps can be open at once in this process
    let space = CrossHeapSpace::reserve().unwrap();
    let second = space.create(dir.file("second"), THREADS).unwrap();
    assert_ne!(second.id(), taken);
    let reopened = space.open(&first, THREADS).unwrap();
    assert_eq!(reopened.id(), taken);
    // The heap file is gone, so its marker is stale: the number is reclaimed and reused
    drop(reopened);
    std::fs::remove_file(&first).unwrap();
    let third = space.create(dir.file("third"), THREADS).unwrap();
    assert_eq!(third.id(), taken, "the heap number of a stale marker is reclaimed and reused");
    drop((second, third));
    let left: Vec<_> = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "after the creating handles were dropped, files and markers were deleted: {left:?}");
}

#[test]
fn processes_share_one_heap() {
    const KIDS: usize = 4;
    const ROUNDS_EACH: u64 = 40;
    let p = Parent::new("share");
    let kids: Vec<Kid> = (1..=KIDS).map(|c| p.spawn(Role::Churn, c, ROUNDS_EACH)).collect();
    // The parent changes chain 0 and collects at the same time
    for r in 0..ROUNDS_EACH {
        churn(&p.heap, &p.anchor, 0, PUSH, r % 50);
        check(&p.heap, &p.anchor, 0);
        p.heap.collect();
    }
    let lens: Vec<u64> = kids.into_iter().map(Kid::finish).collect();
    p.heap.collect();
    let mut live = check(&p.heap, &p.anchor, 0);
    for (c, len) in (1..=KIDS).zip(lens) {
        assert_eq!(check(&p.heap, &p.anchor, c), len, "the parent sees the same length for the chain the child changed");
        live += len;
    }
    assert_eq!(p.heap.stats().live, ANCHOR_BYTES + NODE * usize::try_from(live).unwrap(), "live bytes are exactly the chains");
    assert_eq!(p.heap.attached(), 1, "children that exited normally freed their attachment slots");
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
        // The child is alive and stays inside mutate: collection can only wait
        std::thread::sleep(Duration::from_millis(300));
        assert!(!collecting.is_finished(), "the child is still inside mutate, collection must not start");
        kid.kill();
        collecting.join().unwrap();
    });
    assert_eq!(p.heap.stats().collections, before + 1);
    assert_eq!(p.heap.attached(), 1, "the dead child was cleaned up");
    assert_eq!(p.heap.stats().live, ANCHOR_BYTES, "the chain held only by it was collected once its entry was returned");
}

#[test]
fn killed_collector_is_taken_over() {
    const TRIES: usize = 12;
    let p = Parent::new("collector");
    // A somewhat bigger chain so every collection takes a few milliseconds, and the kill most likely lands mid-collection
    churn(&p.heap, &p.anchor, 0, 300_000, 0);
    let mut rng = Rng::new();
    for _ in 0..TRIES {
        let mut kid = p.spawn(Role::CollectForever, 1, 0);
        kid.line(READY);
        std::thread::sleep(Duration::from_millis(5 + rng.below(30)));
        kid.kill();
        // The parent uses the heap as usual: if the collector died, a mutate or a collection takes over
        assert_eq!(check(&p.heap, &p.anchor, 0), 300_000);
        p.heap.collect();
        p.eventually("the dead collector was cleaned up", |p| p.heap.attached() == 1);
    }
    assert!(p.heap.stats().takeovers > 0, "at least one of {TRIES} kills hit mid-collection and was taken over");
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
        // Kill one at random, then start a new one to continue on the same chain (kill first, then start: only one process may change a chain)
        let c = usize::try_from(rng.below(KIDS as u64)).unwrap();
        kids[c].take().expect("every position has a child").kill();
        kids[c] = start(c + 1);
        if i.is_multiple_of(3) {
            p.heap.collect();
        }
    }
    for k in kids.into_iter().flatten() {
        k.kill();
    }
    p.heap.collect();
    p.eventually("every dead child was cleaned up", |p| {
        p.heap.collect();
        p.heap.attached() == 1
    });
    let live: u64 = (0..=KIDS).map(|c| check(&p.heap, &p.anchor, c)).sum();
    assert_eq!(p.heap.stats().live, ANCHOR_BYTES + NODE * usize::try_from(live).unwrap(), "everything of the killed processes was reclaimed and nothing still held is missing");
}

/// The eight-process scenario: process count and chain length in each public / private heap
const PAIRS: usize = 8;
const PAIR_NODES: u64 = 500_000;
/// Commands and replies of the eight-process scenario (line prefixes on standard input / output)
const PAIR_INFO: &str = "PAIR ";
const PEERS: &str = "PEERS ";
const LINKED: &str = "PAIR-LINKED";
const RELEASE: &str = "RELEASE";
const CHECK: &str = "CHECK ";
const CHECKED: &str = "PAIR-OK ";
const EXIT: &str = "EXIT";

/// The "hub" in a public heap: points at the gift for this process in every other process's public heap
#[repr(C)]
#[derive(Shared)]
struct Hub<'p> {
    refs: [CrossHeapRef<'p, NodeB>; PAIRS],
}

cross_heap_brand!(HubB = for<'p> Hub<'p>);

const HUB: usize = (HEADER + size_of::<Hub<'static>>()).next_multiple_of(8);

/// A card in a private heap: holds a handle to the chain in another process's public heap
#[derive(Trace)]
struct Card {
    peer: usize,
    chain: CrossHeapRoot<NodeB>,
}

brand!(CardB = for<'gc> Card);

/// A chain in the private heap
#[derive(Trace)]
struct Bead<'gc> {
    value: u64,
    next: Option<paragc::Gc<'gc, Bead<'gc>>>,
}

brand!(BeadB = for<'gc> Bead<'gc>);

/// A gift's value: whom it is for and who gave it
fn gift_value(from: usize, to: usize) -> u64 {
    1_000_000 + (from * PAIRS + to) as u64
}

/// One process of the eight-process scenario.
struct Pair {
    me: usize,
    mine: CrossHeap,
    chain: CrossHeapRoot<NodeB>,
    hub: CrossHeapRoot<HubB>,
    private: Heap,
    beads: Root<BeadB>,
    /// The other processes' public heaps (dead ones dropped)
    peers: Vec<Option<CrossHeap>>,
    cards: Vec<Option<Root<CardB>>>,
}

impl Pair {
    /// The child's side: create the heaps, report, wait for PEERS, connect to the others, wait for RELEASE, report READY, then work while listening for commands.
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
        // A gift for every other process: once its hub points here and this process drops its handle, it lives only through that reference
        let gifts: Vec<Option<CrossHeapRoot<NodeB>>> = (0..PAIRS).map(|j| (j != me).then(|| mine.mutate(|m| m.root(m.alloc(Node { value: gift_value(me, j), next: CrossHeapSlot::default() }))))).collect();
        let gift_list: Vec<String> = gifts.iter().map(|g| g.as_ref().map_or(u32::MAX, CrossHeapRoot::index).to_string()).collect();
        println!("{PAIR_INFO}{me} {} {}", chain.index(), gift_list.join(","));
        let mut stdin = std::io::stdin().lines();
        let peers_line = stdin.next().unwrap().unwrap();
        let peers_line = peers_line.strip_prefix(PEERS).expect("PEERS");
        // Per process: path, chain head entry number, gift entry numbers for each process (comma separated); processes separated by ;
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
                let chain: CrossHeapRoot<NodeB> = h.mutate(|m| m.find(infos[j].1)).expect("the other's chain");
                private.mutate(|m| m.root(m.alloc(Card { peer: j, chain })))
            }));
            // Hub slot j points at the gift j gave this process
            if let Some(h) = peer {
                mine.mutate_with(h, |mi, mj| {
                    let gift: CrossHeapRoot<NodeB> = mj.find(infos[j].2[me]).expect("gift");
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

    /// A background thread keeps allocating, collecting and reading the others; the main thread waits for commands.
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

    /// Background: allocate garbage in its own public heap, collect, and read the live others (skipping those it cannot read).
    fn churn(&self, stop: &std::sync::atomic::AtomicBool) {
        let mut round = 0u64;
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            self.mine.mutate(|m| {
                for value in 0..1_000 {
                    m.alloc(Node { value, next: CrossHeapSlot::default() });
                }
            });
            for (j, peer) in self.peers.iter().enumerate() {
                // Enter two public heaps at once with mutate_with (entering in heap-number order); dead ones are not entered
                let Some(h) = peer.as_ref().filter(|h| h.is_alive()) else { continue };
                self.mine.mutate_with(h, |mi, mj| self.hub.get(mi).refs[j].get(mj).map(|g| g.value));
            }
            if round.is_multiple_of(4) {
                self.mine.collect();
            }
            round += 1;
        }
    }

    /// Check (background stopped): dead others cannot be read and are dropped; live ones read with the right values; its own heap's live bytes are exactly "chain + hub + gifts held by the living". Returns the live bytes of its own public heap.
    fn check(&mut self, dead: &[usize]) -> usize {
        let me = self.me;
        for j in (0..PAIRS).filter(|&j| j != me) {
            let h = self.peers[j].as_ref();
            if dead.contains(&j) {
                if let Some(h) = h {
                    assert!(!h.is_alive(), "process {j} died, so its public heap should be dead too");
                    assert_eq!(h.try_mutate(|_| ()), Err(HeapDead));
                }
                // Drop the handles pointing at it: the card in the private heap, and this process's mapping of it
                self.cards[j] = None;
                self.peers[j] = None;
                continue;
            }
            let h = h.expect("every live one is still open");
            assert!(h.is_alive(), "process {j} is still alive");
            let gift = self.mine.mutate_with(h, |mi, mj| self.hub.get(mi).refs[j].get(mj).map(|g| g.value));
            assert_eq!(gift, Some(gift_value(j, me)), "the hub points at the gift from {j}");
            let card = self.cards[j].as_ref().expect("the live card is still there");
            let head = self.private.mutate(|m| h.try_mutate(|mj| m.get(card).chain.get(mj).value)).expect("a live one can be read");
            assert_eq!(head, PAIR_NODES - 1);
            assert_eq!(self.private.mutate(|m| m.get(card).peer), j);
        }
        // Private heap: cards are finalized (their handles return entries), and the bead chain is intact
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
        // Its own public heap: gift registrations held by the dead are returned, and those gifts are collected
        self.mine.collect();
        let alive_peers = (0..PAIRS).filter(|&j| j != me && !dead.contains(&j)).count();
        let expected = NODE * usize::try_from(PAIR_NODES).unwrap() + HUB + NODE * alive_peers;
        let live = self.mine.stats().live;
        assert_eq!(live, expected, "process {me}: live = chain + hub + {alive_peers} gifts");
        // Collecting together with the live others still works
        let mut group = vec![&self.mine];
        group.extend(self.peers.iter().flatten());
        CrossHeap::collect_group(&group);
        assert_eq!(self.mine.mutate(|m| self.chain.get(m).value), PAIR_NODES - 1);
        live
    }
}

/// The process's peak memory and /dev/shm usage (meaningful when running in a cgroup).
fn report_memory(what: &str) {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let dir = cgroup.trim().rsplit(':').next().unwrap_or_default();
    let read = |f: &str| std::fs::read_to_string(format!("/sys/fs/cgroup{dir}/{f}")).map_or_else(|_| "?".into(), |v| v.trim().to_owned());
    let shm = std::fs::read_to_string("/proc/meminfo").unwrap_or_default().lines().find(|l| l.starts_with("Shmem:")).unwrap_or_default().to_owned();
    eprintln!("[memory] {what}: cgroup memory.max={} memory.peak={} memory.current={}; {shm}", read("memory.max"), read("memory.peak"), read("memory.current"));
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
    report_memory("all eight processes are up");
    // Let the background run a while, then everyone checks once (nobody dead)
    std::thread::sleep(Duration::from_millis(200));
    for k in kids.iter_mut().flatten() {
        k.send(CHECK);
    }
    for k in kids.iter_mut().flatten() {
        k.line(CHECKED);
    }
    // Kill half while the background is still running
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
    report_memory("after four died");
    // Dead heaps: file and marker deleted; live ones remain
    for i in 0..PAIRS {
        let alive = !victims.contains(&i);
        assert_eq!(dir.file(&format!("heap-{i}")).exists(), alive, "heap {i}'s file");
    }
    let markers = std::fs::read_dir(&dir.0).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with(".paragc-")).count();
    assert_eq!(markers, PAIRS - victims.len(), "the markers of dead heaps were deleted");
    for k in kids.iter_mut().flatten() {
        k.send(EXIT);
    }
    for k in kids.into_iter().flatten() {
        assert!(k.child.wait_with_output().unwrap().status.success());
    }
    // The living have exited too: their heaps died with them and the directory is empty
    let left: Vec<_> = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "every process left and every heap died with it: {left:?}");
}

#[test]
fn simultaneous_creates_get_distinct_numbers() {
    // Create heaps in the same directory at once: the number marker, heap file and superblock are written in several steps, and someone reading an intermediate state must not take it for a stale marker
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
        assert!(unique, "round {round}: heap numbers created at the same time collided: {numbers:?}");
    }
}
