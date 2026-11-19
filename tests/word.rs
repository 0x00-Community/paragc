//! `WordSlot`：槽里放使用者自己编码的字（指针字带自己的标签，别的字是数），经过各种回收后标签、数原样不动，指着的对象活着。
//! 逐条跑：`cargo test --test word -- --exact <名字>`
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use paragc::{Gc, Heap, Root, Trace, Tracer, WORD_PAYLOAD, WordSlot, YoungEvacuation, brand, word_pointer};

/// 析构时计数（没有 Gc，可以有 Drop）。
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: 没有 Gc
unsafe impl Trace for Counted {
    const NO_EDGES: bool = true;
    fn trace(&self, _: &mut Tracer<'_>) {}
}

/// 链上一个节点：`next` 是指针字（标签 [`TAG`]）或数字字（链尾），`n` 永远是数字字
#[derive(Trace)]
struct Node<'gc> {
    next: WordSlot<'gc>,
    n: WordSlot<'gc>,
    tag: Counted,
}

brand!(NodeB = for<'gc> Node<'gc>);

/// 使用者自己的指针标签（指针区间里、不是 paragc 的 `RAW_TAG`）
const TAG: u64 = 0xFFF9 << 48;
/// 链尾放的数（f64 的位：数字字落在指针区间之外）
const END: f64 = -1.5;

const THREADS: NonZeroUsize = NonZeroUsize::new(if cfg!(miri) { 2 } else { 4 }).unwrap();
const LEN: usize = if cfg!(miri) { 100 } else { 10_000 };

fn pointer(n: Gc<'_, Node<'_>>) -> u64 {
    TAG | n.as_ptr().expose_provenance() as u64
}

/// 字里的节点（标签必须原样）
fn node<'gc>(word: u64) -> Option<Gc<'gc, Node<'gc>>> {
    let p = word_pointer(word)?;
    assert_eq!(word & !WORD_PAYLOAD, TAG, "标签变了");
    // SAFETY: 字是 `pointer` 造的，节点被链上一个可达的节点指着
    Some(unsafe { Gc::from_raw(p.cast::<Node<'gc>>()) })
}

fn number(n: f64) -> u64 {
    n.to_bits()
}

/// 头节点一个根，后面 `len` 个节点；节点 i 的 `n` 是 i
fn build(heap: &Heap, len: usize, drops: &Arc<AtomicUsize>) -> Root<NodeB> {
    heap.mutate(|m| {
        let mut next = number(END);
        for i in (0..len).rev() {
            #[expect(clippy::cast_precision_loss, reason = "小于 2^53")]
            let n = m.alloc(Node { next: WordSlot::new(next), n: WordSlot::new(number(i as f64)), tag: Counted(Arc::clone(drops)) });
            next = pointer(n);
        }
        m.root(node(next).expect("len > 0"))
    })
}

/// 沿链走到底：节点数，`n` 按顺序递增、链尾是 END
fn walk(heap: &Heap, root: &Root<NodeB>) -> usize {
    heap.mutate(|m| {
        let mut cur = Some(m.get(root));
        let mut count = 0;
        let mut last = -1.0;
        while let Some(n) = cur {
            let v = f64::from_bits(n.n.get());
            assert!(v > last, "数字字变了：{v} 不大于 {last}");
            last = v;
            count += 1;
            let next = n.next.get();
            cur = node(next);
            if cur.is_none() {
                assert_eq!(next, number(END), "链尾的数变了");
            }
        }
        count
    })
}

/// 完整回收（含疏散）后链完整、标签与数不变；把中间一个指针字换成数字字，后半截全被回收
#[test]
fn survives_full_collections_and_cut_frees_the_rest() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let root = build(&heap, LEN, &drops);
    for _ in 0..3 {
        heap.collect();
        assert_eq!(walk(&heap, &root), LEN);
    }
    assert_eq!(drops.load(Ordering::Relaxed), 0, "活着的节点被析构了");
    // 在第 LEN / 2 个节点处截断：它的 next 换成数字字
    heap.mutate(|m| {
        let mut cur = m.get(&root);
        for _ in 1..LEN / 2 {
            cur = node(cur.next.get()).expect("链够长");
        }
        cur.next.set(m, number(END));
    });
    heap.collect();
    assert_eq!(walk(&heap, &root), LEN / 2);
    assert_eq!(drops.load(Ordering::Relaxed), LEN - LEN / 2, "截掉的后半截没有全部回收");
    drop(root);
    drop(heap);
    assert_eq!(drops.load(Ordering::Relaxed), LEN);
}

/// 老节点的字里挂上年轻节点（记忆集），年轻回收（三种搬法）后都还在；扔掉的年轻节点被回收
fn young(mode: YoungEvacuation) {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    heap.set_young_evacuation(mode);
    let root = build(&heap, LEN, &drops);
    heap.collect();
    // 每个老节点后面插一个年轻节点；另外造同样多个扔掉
    heap.mutate(|m| {
        let mut cur = Some(m.get(&root));
        while let Some(old) = cur {
            let next = old.next.get();
            let v = f64::from_bits(old.n.get());
            let y = m.alloc(Node { next: WordSlot::new(next), n: WordSlot::new(number(v + 0.5)), tag: Counted(Arc::clone(&drops)) });
            old.next.set(m, pointer(y));
            let _garbage = m.alloc(Node { next: WordSlot::new(pointer(y)), n: WordSlot::new(number(0.0)), tag: Counted(Arc::clone(&drops)) });
            cur = node(next);
        }
    });
    heap.collect_young();
    assert_eq!(walk(&heap, &root), LEN * 2, "年轻回收后链上丢了节点");
    assert_eq!(drops.load(Ordering::Relaxed), LEN, "扔掉的年轻节点没有恰好全部回收");
    heap.collect();
    assert_eq!(walk(&heap, &root), LEN * 2);
    assert_eq!(drops.load(Ordering::Relaxed), LEN);
}

#[test]
fn young_collection_in_place() {
    young(YoungEvacuation::Off);
}

#[test]
fn young_collection_skipping_remembered() {
    young(YoungEvacuation::SkipRemembered);
}

#[test]
fn young_collection_moving_remembered() {
    young(YoungEvacuation::Full);
}

/// 并发标记期间改写字（指针 ⇄ 数），收尾后链完整、计数可信（再截断时按计数也能回收）
#[test]
fn concurrent_marking_while_rewriting() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut heap = Heap::new(THREADS);
    let root = build(&heap, LEN, &drops);
    heap.collect();
    for round in 0..4 {
        assert!(heap.mark_concurrently());
        // 每个节点的 n 先写成指针字（指向下一个节点）再写回数：屏障两种方向都走一遍
        heap.mutate(|m| {
            let mut cur = Some(m.get(&root));
            while let Some(n) = cur {
                let v = n.n.get();
                let next = n.next.get();
                if node(next).is_some() {
                    n.n.set(m, next);
                }
                n.n.set(m, v);
                cur = node(next);
            }
        });
        if round % 2 == 0 { heap.collect_young() } else { heap.collect() }
        assert_eq!(walk(&heap, &root), LEN);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }
    heap.mutate(|m| {
        m.get(&root).next.set(m, number(END));
    });
    heap.collect();
    heap.collect();
    assert_eq!(walk(&heap, &root), 1);
    assert_eq!(drops.load(Ordering::Relaxed), LEN - 1);
}
