//! 公开堆（共享内存上的堆）。逐条跑：`cargo test --test public -- --exact <名字>`
#![cfg(target_os = "linux")]

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use paragc::{Heap, Public, PublicHeap, PublicSlot, PublicSpace, Root, Shared, Trace, brand, public_brand};

/// 每个堆的回收线程数（Miri 下减少）
const THREADS: NonZeroUsize = NonZeroUsize::new(if cfg!(miri) { 2 } else { 4 }).unwrap();
/// 公开堆的对象头
const HEADER: usize = 16;

#[repr(C)]
#[derive(Shared)]
struct Node<'p> {
    value: u64,
    next: PublicSlot<'p, Node<'p>>,
}

public_brand!(NodeB = for<'p> Node<'p>);

/// 一个 Node 占的字节数（头 + 数据，按 8 字节）
const NODE: usize = (HEADER + size_of::<Node<'static>>()).next_multiple_of(8);

/// 建一条 `len` 个节点的链（值 0..len），返回链头的句柄。
fn list(heap: &PublicHeap, len: u64) -> Public<NodeB> {
    heap.mutate(|m| {
        let mut top = None;
        for value in 0..len {
            top = Some(m.alloc(Node { value, next: top.into() }));
        }
        m.root(top.expect("len > 0"))
    })
}

fn sum(heap: &PublicHeap, root: &Public<NodeB>) -> u64 {
    heap.mutate(|m| {
        let (mut sum, mut cur) = (0, Some(root.get(m)));
        while let Some(n) = cur {
            sum += n.value;
            cur = n.next.get(m);
        }
        sum
    })
}

fn triangle(len: u64) -> u64 {
    len * (len - 1) / 2
}

/// 测试用的共享内存文件名（进程号 + 名字，互不冲突）。
fn shm_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/dev/shm/paragc-test-{}-{name}", std::process::id()))
}

#[test]
fn public_list_survives_and_garbage_is_reclaimed() {
    const LEN: u64 = if cfg!(miri) { 300 } else { 20_000 };
    let space = PublicSpace::reserve().unwrap();
    let heap = space.create_anonymous(THREADS).unwrap();
    let kept = list(&heap, LEN);
    drop(list(&heap, LEN));
    heap.collect();
    assert_eq!(sum(&heap, &kept), triangle(LEN));
    assert_eq!(heap.stats().live, NODE * usize::try_from(LEN).unwrap(), "存活字节数恰好是留下的那条链");
    // 腾出来的行被复用：再分配同样多不开新块
    let blocks = heap.stats().blocks;
    drop(list(&heap, LEN));
    assert_eq!(heap.stats().blocks, blocks);
    drop(kept);
    heap.collect();
    assert_eq!(heap.stats().live, 0);
}

/// 嵌套类型与定长数组里的指针槽都要在指针图里
#[repr(C)]
#[derive(Shared)]
struct Inner<'p> {
    tag: u32,
    slot: PublicSlot<'p, Node<'p>>,
}

#[repr(C)]
#[derive(Shared)]
struct Holder<'p> {
    many: [PublicSlot<'p, Node<'p>>; 3],
    bytes: [u8; 5],
    inner: Inner<'p>,
}

public_brand!(HolderB = for<'p> Holder<'p>);

#[test]
fn pointer_map_covers_nested_fields_and_arrays() {
    let space = PublicSpace::reserve().unwrap();
    let heap = space.create_anonymous(THREADS).unwrap();
    let holder: Public<HolderB> = heap.mutate(|m| {
        let leaf = |value| m.alloc(Node { value, next: PublicSlot::default() });
        let h = m.alloc(Holder { many: [leaf(1).into(), leaf(2).into(), leaf(3).into()], bytes: [7; 5], inner: Inner { tag: 9, slot: leaf(4).into() } });
        // 一个没挂上的：会被回收
        leaf(100);
        m.root(h)
    });
    heap.collect();
    let holder_bytes = (HEADER + size_of::<Holder<'static>>()).next_multiple_of(8);
    assert_eq!(heap.stats().live, holder_bytes + 4 * NODE, "四个挂着的叶子都活着，没挂的死了");
    heap.mutate(|m| {
        let h = holder.get(m);
        let values: Vec<u64> = h.many.iter().chain([&h.inner.slot]).map(|s| s.get(m).expect("挂着").value).collect();
        assert_eq!(values, [1, 2, 3, 4]);
        assert_eq!((h.bytes, h.inner.tag), ([7; 5], 9));
        // 剪掉两个
        h.many[1].set(m, None);
        h.inner.slot.set(m, None);
    });
    heap.collect();
    assert_eq!(heap.stats().live, holder_bytes + 2 * NODE);
}

/// 大对象（超过 8 KiB）：单独占一串块
#[repr(C)]
#[derive(Shared)]
struct Big<'p> {
    data: [u64; 2048],
    next: PublicSlot<'p, Big<'p>>,
}

public_brand!(BigB = for<'p> Big<'p>);

#[test]
fn large_public_objects() {
    const COUNT: u64 = if cfg!(miri) { 3 } else { 40 };
    let space = PublicSpace::reserve().unwrap();
    let heap = space.create_anonymous(THREADS).unwrap();
    let chain = |seed: u64| -> Public<BigB> {
        heap.mutate(|m| {
            let mut top = None;
            for i in 0..COUNT {
                top = Some(m.alloc(Big { data: core::array::from_fn(|k| seed + i + k as u64), next: top.into() }));
            }
            m.root(top.expect("COUNT > 0"))
        })
    };
    let kept = chain(1_000);
    drop(chain(5_000));
    heap.collect();
    let big = (HEADER + size_of::<Big<'static>>()).next_multiple_of(8);
    assert_eq!(heap.stats().live, big * usize::try_from(COUNT).unwrap());
    heap.mutate(|m| {
        let (mut cur, mut i) = (Some(kept.get(m)), COUNT);
        while let Some(b) = cur {
            i -= 1;
            assert!(b.data.iter().enumerate().all(|(k, &v)| v == 1_000 + i + k as u64), "大对象的内容完整");
            cur = b.next.get(m);
        }
        assert_eq!(i, 0);
    });
    // 死掉的大对象拆成的块给小对象用：分配小对象不开新块
    let blocks = heap.stats().blocks;
    drop(list(&heap, if cfg!(miri) { 100 } else { 10_000 }));
    assert_eq!(heap.stats().blocks, blocks, "拆开的大对象块被小对象复用");
}

#[test]
#[cfg_attr(miri, ignore = "Miri 跑不了文件映射")]
fn file_starts_empty_and_grows() {
    let path = shm_path("grow");
    let space = PublicSpace::reserve().unwrap();
    let heap = space.create(&path, THREADS).unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    // 建堆只写了超级块那一页
    assert_eq!(meta.len(), 4096, "新文件只有超级块");
    let root = list(&heap, 50_000);
    let grown = std::fs::metadata(&path).unwrap();
    assert!(grown.len() > meta.len(), "分配时文件按需变长");
    {
        use std::os::unix::fs::MetadataExt;
        // 实际占用（块数 × 512）不超过文件长度：中间没用到的段是空洞
        assert!(grown.blocks() * 512 <= grown.len());
    }
    assert_eq!(sum(&heap, &root), triangle(50_000));
    drop(root);
    drop(heap);
    assert!(!path.exists(), "建堆的句柄全部析构后删除文件");
}

/// 每个线程一条自己的链挂在这里
#[repr(C)]
#[derive(Shared)]
struct Anchor<'p> {
    heads: [PublicSlot<'p, Node<'p>>; 8],
}

public_brand!(AnchorB = for<'p> Anchor<'p>);

/// 第 `t` 条链：在头上压 `push` 个节点（值 = 当前长度），再从头上弹掉 `pop` 个；返回新长度。
fn churn(heap: &PublicHeap, anchor: &Public<AnchorB>, t: usize, len: u64, push: u64, pop: u64) -> u64 {
    heap.mutate(|m| {
        let top = &anchor.get(m).heads[t];
        for v in len..len + push {
            let n = m.alloc(Node { value: v, next: top.get(m).into() });
            top.set(m, Some(n));
        }
        for _ in 0..pop {
            let first = top.get(m).expect("链不比弹掉的短");
            top.set(m, first.next.get(m));
        }
        len + push - pop
    })
}

/// 第 `t` 条链从头到尾的值必须是 len-1, len-2, …, 0。
fn check(heap: &PublicHeap, anchor: &Public<AnchorB>, t: usize, len: u64) {
    heap.mutate(|m| {
        let (mut cur, mut expect) = (anchor.get(m).heads[t].get(m), len);
        while let Some(n) = cur {
            expect -= 1;
            assert_eq!(n.value, expect, "链上的值错位：对象被提前回收或指针坏了");
            cur = n.next.get(m);
        }
        assert_eq!(expect, 0, "链长对不上");
    });
}

/// 线程结束（含 panic）时计数，回收线程据此停下：panic 的线程不计数会让它一直转
struct Finished<'a>(&'a AtomicUsize);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri 跑不了文件映射")]
fn two_mappings_of_one_heap() {
    const LEN: u64 = 10_000;
    const ROUNDS: u64 = 30;
    let path = shm_path("two");
    // 同一进程里两个地址区：同一个堆映射在两个不同的基址；指针存的是偏移，两边看到同一个对象图
    let (sa, sb) = (PublicSpace::reserve().unwrap(), PublicSpace::reserve().unwrap());
    let a = sa.create(&path, THREADS).unwrap();
    let b = sb.open(&path, THREADS).unwrap();
    assert_eq!(a.id(), b.id());
    let root = list(&a, LEN);
    let rb: Public<NodeB> = b.mutate(|m| m.find(root.index())).expect("凭入口号找到");
    assert!(b.mutate(|m| m.find::<HolderB>(root.index())).is_none(), "类型对不上时找不到");
    let pa = a.mutate(|m| root.get(m).as_ptr().addr());
    let pb = b.mutate(|m| rb.get(m).as_ptr().addr());
    assert_ne!(pa, pb, "两个映射的地址不同");
    assert_eq!(sum(&b, &rb), triangle(LEN));
    // 两边各 4 个线程改各自的链，另一个线程轮流从两边回收：握手字在超级块里，所以跨映射也互相排斥
    let anchor = a.mutate(|m| m.root::<AnchorB>(m.alloc(Anchor { heads: Default::default() })));
    let anchor_b: Public<AnchorB> = b.mutate(|m| m.find(anchor.index())).expect("凭入口号找到");
    let done = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for t in 0..8 {
            let (heap, anchor, done) = if t % 2 == 0 { (&a, &anchor, &done) } else { (&b, &anchor_b, &done) };
            s.spawn(move || {
                let _finished = Finished(done);
                let mut len = 0;
                for round in 0..ROUNDS {
                    len = churn(heap, anchor, t, len, 200, round % 50);
                    check(heap, anchor, t, len);
                }
            });
        }
        s.spawn(|| {
            let mut k = 0;
            while done.load(Ordering::Acquire) < 8 {
                if k % 2 == 0 { a.collect() } else { b.collect() }
                k += 1;
            }
        });
    });
    // 建堆的一方先放手：文件删掉，但另一个映射照常可用
    drop((root, anchor));
    drop(a);
    assert!(!path.exists());
    b.collect();
    assert_eq!(sum(&b, &rb), triangle(LEN));
}

/// 私有堆的对象里放一个指向公开堆的句柄
#[derive(Trace)]
struct Card {
    public: Public<NodeB>,
    _drops: Counted,
}

brand!(CardB = for<'gc> Card);

struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: 没有 Gc
unsafe impl Trace for Counted {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut paragc::Tracer<'_>) {}
}

#[test]
fn private_heap_holds_public_handles() {
    const LEN: u64 = if cfg!(miri) { 50 } else { 5_000 };
    let space = PublicSpace::reserve().unwrap();
    let public = space.create_anonymous(THREADS).unwrap();
    let mut private = Heap::new(THREADS);
    let drops = Arc::new(AtomicUsize::new(0));
    let card: Root<CardB> = {
        let handle = list(&public, LEN);
        private.mutate(|m| m.root(m.alloc(Card { public: handle, _drops: Counted(drops.clone()) })))
    };
    public.collect();
    assert_eq!(public.stats().live, NODE * usize::try_from(LEN).unwrap(), "私有堆里的句柄是公开堆的根");
    let total = private.mutate(|m| public.mutate(|pm| {
        let (mut s, mut cur) = (0, Some(m.get(&card).public.get(pm)));
        while let Some(n) = cur {
            s += n.value;
            cur = n.next.get(pm);
        }
        s
    }));
    assert_eq!(total, triangle(LEN));
    // 私有堆回收析构 Card ⇒ 句柄还掉入口 ⇒ 公开堆下次回收收掉整条链
    drop(card);
    private.collect();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    public.collect();
    assert_eq!(public.stats().live, 0);
}

#[test]
fn eight_threads_churn_public_heap() {
    const ROUNDS: u64 = if cfg!(miri) { 2 } else { 40 };
    const PUSH: u64 = if cfg!(miri) { 20 } else { 500 };
    let space = PublicSpace::reserve().unwrap();
    let heap = space.create_anonymous(THREADS).unwrap();
    let anchor = heap.mutate(|m| m.root::<AnchorB>(m.alloc(Anchor { heads: Default::default() })));
    let lens: Vec<AtomicU64> = (0..8).map(|_| AtomicU64::new(0)).collect();
    for round in 0..ROUNDS {
        std::thread::scope(|s| {
            for (t, len) in lens.iter().enumerate() {
                let (heap, anchor) = (&heap, &anchor);
                s.spawn(move || {
                    let now = churn(heap, anchor, t, len.load(Ordering::Relaxed), PUSH, (round * 7 + t as u64) % PUSH);
                    len.store(now, Ordering::Relaxed);
                    check(heap, anchor, t, now);
                });
            }
            // 有线程在 mutate 时回收：等它们走空、回收、再放行
            s.spawn(|| heap.collect());
        });
        heap.collect();
        let live: u64 = lens.iter().map(|l| l.load(Ordering::Relaxed)).sum();
        let anchor_bytes = (HEADER + size_of::<Anchor<'static>>()).next_multiple_of(8);
        assert_eq!(heap.stats().live, anchor_bytes + NODE * usize::try_from(live).unwrap(), "存活字节数恰好是各条链");
    }
}

#[test]
#[cfg_attr(miri, ignore = "Miri 跑不了文件映射")]
fn collect_fresh_file_heap_without_roots() {
    // 新文件、一个入口都没有就回收（各段还一项都没用，不能去延长 0 长度），再只分配不挂根地回收
    let path = shm_path("fresh");
    let space = PublicSpace::reserve().unwrap();
    let heap = space.create(&path, THREADS).unwrap();
    heap.collect();
    drop(list(&heap, 1_000));
    heap.collect();
    assert_eq!(heap.stats().live, 0);
    assert_eq!(heap.stats().collections, 2);
}
