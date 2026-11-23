#!/usr/bin/env python3
"""Generates en/src/bits.md, en/src/moving.md and their zh-CN pages, whose diagrams are HTML styled by en/diagrams.css.

Run from mdbook/: python3 diagrams.py .  (edit the texts here, not the generated pages)
"""
import re
import sys
from pathlib import Path

ROOT = Path(sys.argv[1])


def bf(cells, labels=None, end='', exact=False):
    cls = 'bf' + (' exact' if exact else '') + (' ruled' if labels else '')
    out = [f'<div class="{cls}">']
    for n, c in enumerate(cells):
        w, k, name = c[0], c[1], c[2]
        sub = c[3] if len(c) > 3 else ''
        rng = c[4] if len(c) > 4 else ''
        inner = f'<b>{name}</b>'
        if sub:
            inner += f'<small>{sub}</small>'
        if rng:
            inner += f'<i>{rng}</i>'
        if labels and n < len(labels) and labels[n] != '':
            inner += f'<u>{labels[n]}</u>'
        if labels and end and n == len(cells) - 1:
            inner += f'<u class="end">{end}</u>'
        out.append(f'<div class="{k}" style="--w:{w}">{inner}</div>')
    out.append('</div>')
    return ''.join(out)


def ruler(weights, labels, end=''):
    spans = ''.join(f'<span style="--w:{w}">{l}</span>' for w, l in zip(weights, labels))
    return f'<div class="ruler">{spans}<em>{end}</em></div>'


def bfr(cells, labels, end='', exact=False):
    return bf(cells, labels, end, exact)


def dg(cap, *parts, note=''):
    s = f'<div class="dg"><div class="dg-cap">{cap}</div>' + ''.join(parts)
    if note:
        note = re.sub(r'`([^`]+)`', lambda m: '<code>' + m.group(1).replace('<', '&lt;').replace('>', '&gt;') + '</code>', note)
        s += f'<div class="dg-note">{note}</div>'
    return s + '</div>'


def block_grid():
    pattern = ('LLLLLLLLLLLLOOOOLLLLLLFFFFFFFFLLLLLLLLLLLLLLLLOOOOOOOOFFFFLLLLLLLL'
               'LLLLFFFFFFFFFFFFLLLLLLLLOOOOLLLLLLLLLLLLFFFFFFFFLLLLLLLLLLLLLLLLFFFF')[:128]
    cls = {'L': 'live', 'O': 'old', 'F': 'free'}
    return '<div class="blk">' + ''.join(f'<span class="{cls[c]}"></span>' for c in pattern) + '</div>'


def granules(t):
    cells = [(6, 'k1', 'A', '48 B'), (3, 'k2', 'B', '24 B'), (5, 'dim', t['hole'], '40 B'), (12, 'k3', 'C', '96 B'), (6, 'dim', t['hole'], '48 B')]
    starts = {0, 6, 14}
    bits = ''.join(f'<span class="{"on" if i in starts else ""}">{1 if i in starts else 0}</span>' for i in range(32))
    return (bfr(cells, ['0', '48', '72', '112', '208'], '256 B', exact=True)
            + f'<div class="dg-sub">{t["startbits"]}</div><div class="gran bits">' + bits + '</div>')


def chips(rows):
    out = ['<div class="chips">']
    for hexv, name, payload, ptr in rows:
        out.append(f'<div class="chip{" ptr" if ptr else ""}"><b>{hexv}</b><span>{name}</span><small>{payload}</small></div>')
    out.append('</div>')
    return ''.join(out)


def flow(steps):
    out = ['<div class="flow">']
    for i, (title, k, addr, text) in enumerate(steps):
        if i:
            out.append('<div class="arrow">→</div>')
        out.append(f'<div class="step"><h5>{title}</h5><span class="addr {k}">{addr}</span><p>{text}</p></div>')
    out.append('</div>')
    return ''.join(out)


T = {
    'en': dict(
        bits_title='Bit layouts',
        bits_intro='Every word paragc and j8 put in memory, drawn to scale where it helps. Widths are proportional to bits or bytes; single-bit flags are drawn wider so their labels fit.',
        obj_h='A private object',
        obj_p='An object is a 24-byte header, the data, and an optional tail, rounded up to 8 bytes: `(HEADER + size_of::<T>() + tail).next_multiple_of(8)`. A `ScopeLocal` points at the data, not at the header.',
        obj_cap='Memory of one private object',
        data='data <code>T</code>', tail='tail', tailsub='plain bytes, optional',
        gc_here='<code>ScopeLocal</code> points here ↑ at offset 24',
        mark_cap='<code>mark</code> · u32 · bit 31 on the left',
        fwd='moved', busy='being copied', epoch='epoch of the last mark', epochn='0 = young',
        mark_note='FORWARDED and BUSY exist only during a collection that moves objects. The epoch never reaches them: MAX_EPOCH = 2³⁰ − 1.',
        info_cap='<code>info</code> · u32',
        large='large', parity='count parity', rc='reference count', rcn='sticks at 63', size='size in granules', sizen='× 8 B, header included',
        visit_sub='trace / relocate / ask', drop_sub='destructor, or the new address after a move',
        cross_h='A cross-heap object',
        cross_p='The header is 16 bytes and holds no code pointer: another process may read and collect the object without linking its type. `mark` and `info` are the same two words, so blocks, lines and sweeping share their code with the private heap.',
        cross_cap='Memory of one cross-heap object',
        ty_sub='type-table index', stamp_sub='birth stamp',
        swap_h='Promotion swaps the header',
        swap_p='When a private object moves into a cross-heap, its 24-byte header becomes a 16-byte one. The data and the tail are copied byte for byte.',
        before='private', after='cross-heap',
        blk_h='A block and its metadata',
        blk_p='Every 32 KiB block has 128 lines of 256 bytes. Each line has a `u32` mark: the epoch of the last collection that found something alive in it. Allocation bumps through runs of free lines.',
        blk_cap='One block: 128 lines, 16 per row',
        live='marked in the last finished epoch (live)', oldl='older mark: free again', free='never used',
        gran_h='Zoom into one line',
        gran_p='A line is 32 granules of 8 bytes. The **start bitmap** has one bit per granule, set at the first granule of each object; sweeping and cross-heap lookups walk it. 512 bytes of line marks plus 512 bytes of start bits per block.',
        gran_cap='One 256-byte line: objects A, B, C (6, 3 and 12 granules) and two holes', hole='hole', startbits='start bitmap: one bit per granule, set where an object begins',
        slot_h='Slots and words',
        slot_cap='<code>Slot&lt;\'scope, T&gt;</code> · two words', slot_v='value', slot_vs='current pointer', slot_c='last counted', slot_cs='for reference counting',
        word_cap='a <code>WordSlot</code> value word', word_tag='tag', word_tags='≥ 0xFFF8: a pointer', word_addr='data address', word_addrs='low 48 bits',
        word_note='Words below `0xFFF8 << 48` are plain data. The tag of a pointer word is kept as is when the object moves.',
        val_h="j8's JS values",
        val_p='A JS `Value` is one NaN-boxed 64-bit word. A double is stored as it is. Every other value puts `0xFFF` in the top 12 bits, the kind in the next 4, and a payload in the low 48.',
        dbl_cap='a double', sign='sign', exp='exponent', frac='fraction',
        box_cap='any other value', kind='kind + 1', payload='payload', payloads='address, index or bit',
        chips_cap='Top 16 bits by kind (blue: points into the heap)',
        k_undef='undefined', k_null='null', k_bool='boolean', k_uninit='uninitialised (TDZ)', k_lazy='lazy value', k_str='string', k_sym='symbol', k_big='bigint', k_obj='object', k_acc='accessor pair', k_ctx='context',
        p_none='no payload', p_bit='0 or 1', p_idx='index', p_addr='address',
        val_note='Every kind that points into the heap starts at `0xFFF8`, exactly where `WordSlot` starts treating a word as a pointer. A JS value in a `WordSlot` (j8\'s `ValueCell`) is therefore an edge the collector traces, counts and moves, and the kind survives the move.',
        xw_h='Cross-heap words',
        cs_cap='<code>CrossHeapSlot</code> · u64', cs='offset from the start of the region', css='the same in every process',
        cw_cap='<code>CrossHeapWeak</code> · u64', cw_off='offset ÷ 8', cw_offs='1 TiB region: 40 bits − 3', cw_st='stamp', cw_sts='low 27 bits',
        sig_cap='<code>Signature</code> · 32 bytes', sig_w='weak word', sig_i='heap incarnation', sig_k='type key',
        str_h='Example: a JS string',
        str_p='`JsStr` is 32 bytes of fields with its text in the tail. A 40-byte ASCII string takes 96 bytes, 12 granules, in a private heap, and 88 bytes once promoted.',
        str_cap='"…40 ASCII bytes…" in a private heap',
        hdr='header', buf_s='none, or the buffer a view reads', len_s='UTF-8 bytes', units_s='UTF-16 units', start_s='view start', cap_s='room to append', used_s='claimed, atomic', pad='pad', text='text', texts='UTF-8',
        str_note='A non-ASCII string continues the tail with padding to 2 and its UTF-16 code units.',
        # moving chapter
        mv_title='Why object addresses change',
        mv_intro='From the outside, the first surprise with paragc is that the same object can have a different address after a collection. This chapter explains it from the user\'s side: what you see, when it happens, why paragc does it, and what you can and cannot keep across a collection.',
        mv_see_h='What you see',
        mv_code='''```rust
let root = heap.scope(|m| m.global(m.alloc(Node { value: 1, next: None })));
let before = heap.scope(|m| m.scope_in(&root).as_ptr() as usize);
heap.collect_young();
let after = heap.scope(|m| m.scope_in(&root).as_ptr() as usize);
assert_ne!(before, after);            // usually true: the survivor was copied
assert_eq!(heap.scope(|m| m.scope_in(&root).value), 1);   // the object is the same
```''',
        mv_see_p='The `ScopeGlobal` still finds the object and the value is unchanged. Only the address moved.',
        mv_life_h='The life of one object',
        mv_life_cap='One object, four addresses',
        s1='Allocated', s1t='young, in a block the thread bump-allocates into; mark = 0',
        s2='Young collection', s2t='it survived, so it is copied into a compact block and becomes old',
        s3='Full collection', s3t='its block became sparse, so evacuation copies it out',
        s4='Promoted', s4t='a cross-heap is attached and the type is shareable: copied into shared memory, where it never moves again',
        mv_when_h='When paragc moves objects',
        mv_when_tbl='''| Event | What moves | Setting |
| --- | --- | --- |
| young collection | young survivors | `SkipRemembered` (default): all except those an old object or another heap's `Far` points at directly · `Full`: all · `Off`: none |
| stop-the-world full collection | live objects in sparse blocks (under half live) | always on |
| young collection with a cross-heap attached | shareable survivors, into the cross-heap | `alloc_shared` types only |

Never moved: anything inside a scope that does not collect; anything during concurrent marking or group collections; large objects (over 16 KiB); targets of `Far`; objects already in a cross-heap.''',
        mv_why_h='Why move at all',
        mv_why='''Moving costs a copy, so paragc only does it when it buys something you notice:

- **Allocation stays a pointer bump.** Copying the survivors of a young collection together leaves whole blocks empty, and the next allocations bump through them without searching.
- **Memory stays bounded.** Without moving, a heap whose live objects are scattered keeps every half-empty block. In the fragmentation benchmark paragc ends at 111 MB RSS and cppgc, which does not move ordinary objects, at 445 MB for the same ~80 MB of live data.
- **Young garbage costs nothing.** Only survivors are touched; the dead are never visited.
- **Sharing stays lazy.** Promotion is a move: an object becomes visible to other processes by being copied into the cross-heap after it has proved it lives, so nothing has to decide at allocation time.''',
        mv_fix_h='How references follow the move',
        mv_fix_p='The collector leaves a forwarding address in the old copy and rewrites every reference it knows about:',
        fw_cap='The old copy after a move',
        fw_mark='FORWARDED set', fw_drop='new address', fw_rest='old data, never read again',
        mv_fix_list='''- **`ScopeLocal` handles** cannot exist during a move: they are borrows of their scope, and a moving collection takes `&mut` the heap or the scope.
- **Edges inside objects** (`ScopeLocal`, `Slot`, `WordSlot` fields) are rewritten through `Trace::relocate`, which `#[derive(Trace)]` generates.
- **Roots, weak references and `Far` entries** live in tables the collector owns, and are rewritten there.
- **Remembered slots** of old objects are rewritten in `Full` mode; in `SkipRemembered` mode their targets simply do not move.''',
        mv_keep_h='What you can keep across a collection',
        mv_keep_tbl='''| Keep | Why it stays valid |
| --- | --- |
| `ScopeGlobal<'heap, 'heap, B>`, `Weak<B>` | the collector rewrites their table slots |
| fields of heap objects | rewritten by `Trace::relocate` |
| the tag bits of a `WordSlot` word | only the low 48 bits change |
| `CrossHeapSlot`, `CrossHeapGlobal::index`, `CrossHeapWeak`, `Signature` | they name cross-heap objects, which never move |
| values copied out of the object | they are yours |''',
        mv_break_h='What breaks, and what to do instead',
        mv_break_tbl='''| Pattern | Why it breaks | Instead |
| --- | --- | --- |
| `as_ptr()` saved in a Rust struct, used after a collection | the object may have moved; the old address may now hold another object | keep a `ScopeGlobal` (or `Weak`) and call `s.scope_in` |
| a hash map keyed by address | the key is stale after a move | hash the content or a stable id; if it must be the address, rehash in `relocate` (below) |
| comparing addresses from before and after a collection | the same object can have two addresses | compare between collections, or compare a stable id (a handle) |
| handing an address to C, the OS or another thread that outlives the scope | nobody rewrites it | copy the bytes out; a pinned space is planned (principle 1: it will still be collected) |
| addresses baked into generated code | the code is not an edge the collector knows | refer to the target through a handle and bake its id (j8's shapes), or make the code a traced object |''',
        mv_j8_h='In j8',
        mv_j8='''- **`===` on objects** compares the NaN-boxed words. Both operands are read in the same turn, inside one `scope`, so they agree.
- **`Map` and `Set`** index objects by address (`MapKey::Ref`). `Collection::relocate` recomputes the key of every object entry after a move and re-inserts it. That is the "rehash in relocate" pattern above.
- **Shapes** are compared in inline caches and baked into JIT code. They are GC objects referenced through [handles](handles.md): moving a shape rewrites only its handle entry, so the id that caches and machine code compare never changes.
- **Strings** move freely: nothing outside the heap keeps their address, and their bytes travel in the tail.''',
    ),
}

T['zh-CN'] = dict(
    bits_title='比特布局',
    bits_intro='paragc 和 j8 放进内存的每个字，必要处按比例画出。宽度和位数或字节数成比例；单个位的标志画宽一些，好放得下标签。',
    obj_h='私有对象',
    obj_p='一个对象由 24 字节的头、数据和可选的尾巴组成，向上取整到 8 字节：`(HEADER + size_of::<T>() + tail).next_multiple_of(8)`。`ScopeLocal` 指向数据，不指向头。',
    obj_cap='一个私有对象的内存',
    data='数据 <code>T</code>', tail='尾巴', tailsub='纯字节，可选',
    gc_here='<code>ScopeLocal</code> 指在这里 ↑ 偏移 24',
    mark_cap='<code>mark</code> · u32 · 第 31 位在最左',
    fwd='已搬走', busy='正在复制', epoch='最后一次标记的纪元', epochn='0 = 年轻',
    mark_note='FORWARDED 和 BUSY 只在会搬对象的回收期间出现。纪元永远到不了它们：MAX_EPOCH = 2³⁰ − 1。',
    info_cap='<code>info</code> · u32',
    large='大对象', parity='计数奇偶', rc='引用计数', rcn='到 63 粘住', size='粒数', sizen='× 8 B，含头',
    visit_sub='追踪 / 改写 / 询问', drop_sub='析构，搬走后存新地址',
    cross_h='跨堆对象',
    cross_p='头是 16 字节，不存代码指针：别的进程不链接这个类型也能读、能回收这个对象。`mark` 和 `info` 是同样的两个字，所以块、行、清扫和私有堆共用代码。',
    cross_cap='一个跨堆对象的内存',
    ty_sub='类型表编号', stamp_sub='出生戳',
    swap_h='晋升时换头',
    swap_p='私有对象搬进跨堆时，24 字节的头换成 16 字节的头，数据和尾巴逐字节拷过去。',
    before='私有', after='跨堆',
    blk_h='块与它的元数据',
    blk_p='每个 32 KiB 的块有 128 行，每行 256 字节。每行一个 `u32` 标记：最后一次在这一行里发现活对象的回收的纪元。分配时在连续的空闲行里递增推进。',
    blk_cap='一个块：128 行，每排 16 行',
    live='带上一次完成的纪元（活）', oldl='标记更旧：重新空闲', free='从没用过',
    gran_h='放大一行',
    gran_p='一行是 32 个 8 字节的粒。**起点位图**每个粒一位，在每个对象的第一个粒置位；清扫和跨堆查找都靠走它。每个块的行标记 512 字节，起点位也是 512 字节。',
    gran_cap='一行 256 字节：对象 A、B、C（6、3、12 个粒）和两个空洞', hole='空洞', startbits='起点位图：每个粒一位，对象开始的地方置 1',
    slot_h='槽与字',
    slot_cap='<code>Slot&lt;\'scope, T&gt;</code> · 两个字', slot_v='值', slot_vs='当前指针', slot_c='上次计数的值', slot_cs='引用计数用',
    word_cap='<code>WordSlot</code> 的值字', word_tag='标签', word_tags='≥ 0xFFF8 即指针', word_addr='数据地址', word_addrs='低 48 位',
    word_note='小于 `0xFFF8 << 48` 的字是普通数据。对象搬动时，指针字的标签原样保留。',
    val_h='j8 的 JS 值',
    val_p='JS 的 `Value` 是一个 NaN-boxing 的 64 位字。double 原样存放；其余的值高 12 位是 `0xFFF`，接下来 4 位是种类，低 48 位是载荷。',
    dbl_cap='一个 double', sign='符号', exp='指数', frac='尾数',
    box_cap='其余的值', kind='种类 + 1', payload='载荷', payloads='地址、编号或一位',
    chips_cap='各种类的高 16 位（蓝色：指向堆）',
    k_undef='undefined', k_null='null', k_bool='布尔', k_uninit='未初始化（TDZ）', k_lazy='惰性值', k_str='字符串', k_sym='symbol', k_big='bigint', k_obj='对象', k_acc='访问器对', k_ctx='上下文',
    p_none='无载荷', p_bit='0 或 1', p_idx='编号', p_addr='地址',
    val_note='所有指向堆的种类都从 `0xFFF8` 开始，正好是 `WordSlot` 开始把字当成指针的位置。所以存在 `WordSlot`（j8 的 `ValueCell`）里的 JS 值就是一条边：回收器追踪它、给它计数、搬动它，种类原样保留。',
    xw_h='跨堆用的字',
    cs_cap='<code>CrossHeapSlot</code> · u64', cs='相对地址区起点的偏移', css='每个进程都一样',
    cw_cap='<code>CrossHeapWeak</code> · u64', cw_off='偏移 ÷ 8', cw_offs='1 TiB 地址区：40 位 − 3', cw_st='出生戳', cw_sts='低 27 位',
    sig_cap='<code>Signature</code> · 32 字节', sig_w='弱引用字', sig_i='堆化身号', sig_k='类型键',
    str_h='例子：一个 JS 字符串',
    str_p='`JsStr` 是 32 字节的字段，文本放在尾巴里。40 字节的 ASCII 字符串在私有堆里占 96 字节、12 个粒；晋升后占 88 字节。',
    str_cap='私有堆里的「…40 个 ASCII 字节…」',
    hdr='头', buf_s='空，或视图读的缓冲区', len_s='UTF-8 字节数', units_s='UTF-16 码元数', start_s='视图起点', cap_s='可续写的空间', used_s='已认领，原子', pad='填充', text='文本', texts='UTF-8',
    str_note='非 ASCII 字符串的尾巴后面，对齐到 2 字节，再接 UTF-16 码元。',
    mv_title='对象地址为什么会变',
    mv_intro='从外面用 paragc，第一个意外是：同一个对象在一次回收之后地址可能不同。本章从使用者的角度解释：你会看到什么、什么时候发生、paragc 为什么这样做、哪些东西能跨回收保存、哪些不能。',
    mv_see_h='你会看到什么',
    mv_code='''```rust
let root = heap.scope(|m| m.global(m.alloc(Node { value: 1, next: None })));
let before = heap.scope(|m| m.scope_in(&root).as_ptr() as usize);
heap.collect_young();
let after = heap.scope(|m| m.scope_in(&root).as_ptr() as usize);
assert_ne!(before, after);            // 通常成立：存活者被复制走了
assert_eq!(heap.scope(|m| m.scope_in(&root).value), 1);   // 对象还是那个对象
```''',
    mv_see_p='`ScopeGlobal` 仍然找得到这个对象，值也没变，变的只是地址。',
    mv_life_h='一个对象的一生',
    mv_life_cap='一个对象，四个地址',
    s1='分配', s1t='年轻，在线程正在递增分配的块里；mark = 0',
    s2='年轻回收', s2t='它活下来了，被复制进紧凑的块，变老',
    s3='完整回收', s3t='它所在的块变稀疏了，疏散把它复制出去',
    s4='晋升', s4t='挂着跨堆、类型可共享：复制进共享内存，从此不再搬动',
    mv_when_h='paragc 什么时候搬对象',
    mv_when_tbl='''| 事件 | 搬什么 | 设置 |
| --- | --- | --- |
| 年轻回收 | 年轻存活者 | `SkipRemembered`（缺省）：除了老对象或别的堆的 `Far` 直接指着的，都搬 · `Full`：全搬 · `Off`：不搬 |
| 全停的完整回收 | 稀疏块（活数据不到一半）里的活对象 | 总是开启 |
| 挂着跨堆时的年轻回收 | 可共享的存活者，搬进跨堆 | 只限 `alloc_shared` 的类型 |

从不搬：不回收的 scope 里的任何对象；并发标记、一起回收期间的任何对象；大对象（超过 16 KiB）；`Far` 的目标；已经在跨堆里的对象。''',
    mv_why_h='为什么要搬',
    mv_why='''搬动要付一次复制的代价，所以 paragc 只在它换来你能感觉到的好处时才搬：

- **分配一直是推一下指针。** 把年轻回收的存活者复制到一起，就空出整块整块的内存，后面的分配直接递增推进，不用搜索。
- **内存有上限。** 不搬的话，活对象散落各处的堆会留着每一个半空的块。在碎片化基准里，同样约 80 MB 的活数据，paragc 结束时 RSS 是 111 MB，不搬普通对象的 cppgc 是 445 MB。
- **年轻垃圾零成本。** 只碰存活者，死掉的对象根本不会被访问。
- **共享仍然是惰性的。** 晋升就是一次搬动：对象证明自己活得久之后，才被复制进跨堆、让别的进程看见，分配时什么都不用决定。''',
    mv_fix_h='引用怎么跟着搬',
    mv_fix_p='回收器在旧副本里留下转发地址，并改写它知道的每一个引用：',
    fw_cap='搬动之后的旧副本',
    fw_mark='置 FORWARDED', fw_drop='新地址', fw_rest='旧数据，不会再读',
    mv_fix_list='''- **`ScopeLocal`** 在搬动时不可能存在：它是对所属 scope 的借用，而会搬对象的回收要取 `&mut` 堆或这个 scope。
- **对象里的边**（`ScopeLocal`、`Slot`、`WordSlot` 字段）经 `Trace::relocate` 改写，`#[derive(Trace)]` 会生成它。
- **根、弱引用、`Far` 入口**放在回收器自己的表里，在表里改写。
- **老对象被记下的槽**在 `Full` 模式下改写；在 `SkipRemembered` 模式下，它们的目标干脆不搬。''',
    mv_keep_h='哪些东西能跨回收保存',
    mv_keep_tbl='''| 保存 | 为什么仍然有效 |
| --- | --- |
| `ScopeGlobal<'heap, 'heap, B>`、`Weak<B>` | 回收器改写它们的表槽 |
| 堆对象的字段 | 由 `Trace::relocate` 改写 |
| `WordSlot` 字的标签位 | 只有低 48 位会变 |
| `CrossHeapSlot`、`CrossHeapGlobal::index`、`CrossHeapWeak`、`Signature` | 它们指的是跨堆对象，而跨堆对象从不搬 |
| 从对象里复制出来的值 | 它们归你了 |''',
    mv_break_h='哪些会坏，该怎么做',
    mv_break_tbl='''| 写法 | 为什么会坏 | 改成 |
| --- | --- | --- |
| 把 `as_ptr()` 存进 Rust 结构，回收之后再用 | 对象可能搬走了，旧地址上可能已经是别的对象 | 存 `ScopeGlobal`（或 `Weak`），用 `s.scope_in` 取 |
| 按地址做键的哈希表 | 搬动后键就过期了 | 按内容或稳定的编号哈希；非用地址不可，就在 `relocate` 里重新哈希（见下） |
| 比较回收前后拿到的地址 | 同一个对象可能有两个地址 | 在两次回收之间比较，或比较稳定的编号（Handle） |
| 把地址交给 C、操作系统，或活得比 scope 久的线程 | 没有人会改写它 | 把字节复制出去；钉住空间在规划中（宗旨第 1 条：照样会被回收） |
| 把地址写死在生成的代码里 | 那段代码不是回收器认识的边 | 经 handle 引用目标、把 id 写进代码（j8 的 shape），或把代码做成被追踪的对象 |''',
    mv_j8_h='在 j8 里',
    mv_j8='''- **对象的 `===`** 比较的是 NaN-boxing 的字。两个操作数在同一轮、同一次 `scope` 里读出，结果一致。
- **`Map` 和 `Set`** 按地址索引对象（`MapKey::Ref`）。`Collection::relocate` 在搬动后重算每个对象键并重新插入，就是上面说的「在 relocate 里重新哈希」。
- **shape** 在内联缓存里被比较、被写死在 JIT 代码里。它们是经 [handle](handles.md) 引用的 GC 对象：shape 搬家只改写它的 handle 表项，缓存和机器码比较的 id 永远不变。
- **字符串**随便搬：堆外没有谁记着它们的地址，它们的字节在尾巴里跟着走。''',
)


def bits_page(t):
    L = []
    L.append(f'# {t["bits_title"]}\n\n{t["bits_intro"]}\n')
    L.append(f'## {t["obj_h"]}\n\n{t["obj_p"]}\n')
    L.append(dg(t['obj_cap'],
                bf([(4, 'k1', 'mark', '4 B'), (4, 'k2', 'info', '4 B'), (8, 'k3', 'visit', t['visit_sub']), (8, 'k4', 'drop', t['drop_sub']),
                    (14, 'k5', t['data'], 'size_of::&lt;T&gt;()'), (8, 'k6 dim', t['tail'], t['tailsub'])], ['0', '4', '8', '16', '24 ▲ ScopeLocal', '24 + T'], 'end'),
                '', note=t['gc_here']) + '\n')
    L.append(dg(t['mark_cap'],
                bf([(3, 'k7', 'FORWARDED', t['fwd'], '31'), (3, 'k3', 'BUSY', t['busy'], '30'), (26, 'k1', 'epoch', t['epoch'] + ' · ' + t['epochn'], '29 … 0')]),
                note=t['mark_note']) + '\n')
    L.append(dg(t['info_cap'],
                bf([(3, 'k7', 'L', t['large'], '31'), (3, 'k3', 'P', t['parity'], '30'), (6, 'k4', 'RC', t['rc'] + ' · ' + t['rcn'], '29 … 24'), (20, 'k2', 'granules', t['size'] + ' · ' + t['sizen'], '23 … 0')])) + '\n')
    L.append(f'## {t["cross_h"]}\n\n{t["cross_p"]}\n')
    L.append(dg(t['cross_cap'],
                bf([(4, 'k1', 'mark', '4 B'), (4, 'k2', 'info', '4 B'), (4, 'k3', 'ty', t['ty_sub']), (4, 'k4', 'stamp', t['stamp_sub']), (14, 'k5', t['data'], 'size_of::&lt;T&gt;()'), (8, 'k6 dim', t['tail'], t['tailsub'])], ['0', '4', '8', '12', '16', '16 + T'], 'end'),
                '') + '\n')
    L.append(f'## {t["swap_h"]}\n\n{t["swap_p"]}\n')
    L.append(dg('', '<div class="swap">'
                + f'<span>{t["before"]}</span>' + bf([(24, 'k1', 'Header', '24 B'), (32, 'k5', 'T', ''), (16, 'k6', t['tail'], '')])
                + f'<span>{t["after"]}</span>' + bf([(16, 'k3', 'PHeader', '16 B'), (32, 'k5', 'T', ''), (16, 'k6', t['tail'], ''), (8, 'dim', '', '')])
                + '</div>') + '\n')
    L.append(f'## {t["blk_h"]}\n\n{t["blk_p"]}\n')
    L.append(dg(t['blk_cap'], block_grid(),
                f'<div class="legend"><span class="live">{t["live"]}</span><span class="old">{t["oldl"]}</span><span class="free">{t["free"]}</span></div>') + '\n')
    L.append(f'## {t["gran_h"]}\n\n{t["gran_p"]}\n')
    L.append(dg(t['gran_cap'], granules(t)) + '\n')
    L.append(f'## {t["slot_h"]}\n')
    L.append(dg(t['slot_cap'], bf([(1, 'k1', t['slot_v'], t['slot_vs'], '8 B'), (1, 'k2', t['slot_c'], t['slot_cs'], '8 B')])) + '\n')
    L.append(dg(t['word_cap'], bf([(16, 'k3', t['word_tag'], t['word_tags'], '63 … 48'), (48, 'k1', t['word_addr'], t['word_addrs'], '47 … 0')]), note=t['word_note']) + '\n')
    L.append(f'## {t["val_h"]}\n\n{t["val_p"]}\n')
    L.append(dg(t['dbl_cap'], bf([(3, 'k7', 'S', t['sign'], '63'), (11, 'k3', 'exp', t['exp'], '62 … 52'), (50, 'k2', 'fraction', t['frac'], '51 … 0')])) + '\n')
    L.append(dg(t['box_cap'], bf([(12, 'k7', '0xFFF', '', '63 … 52'), (4, 'k4', 'kind', t['kind'], '51 … 48'), (48, 'k1', 'payload', t['payloads'], '47 … 0')])) + '\n')
    rows = [('0xFFF1', t['k_undef'], t['p_none'], False), ('0xFFF2', t['k_null'], t['p_none'], False), ('0xFFF3', t['k_bool'], t['p_bit'], False),
            ('0xFFF5', t['k_uninit'], t['p_none'], False), ('0xFFF6', t['k_lazy'], t['p_idx'], False), ('0xFFF8', t['k_str'], t['p_addr'], True),
            ('0xFFF9', t['k_sym'], t['p_addr'], True), ('0xFFFA', t['k_big'], t['p_addr'], True), ('0xFFFB', t['k_obj'], t['p_addr'], True),
            ('0xFFFC', t['k_acc'], t['p_addr'], True), ('0xFFFD', t['k_ctx'], t['p_addr'], True)]
    L.append(dg(t['chips_cap'], chips(rows), note=t['val_note']) + '\n')
    L.append(f'## {t["xw_h"]}\n')
    L.append(dg(t['cs_cap'], bf([(64, 'k1', 'offset', t['cs'] + ' · ' + t['css'], '63 … 0')])) + '\n')
    L.append(dg(t['cw_cap'], bf([(37, 'k1', 'offset / 8', t['cw_offs'], '63 … 27'), (27, 'k4', 'stamp', t['cw_sts'], '26 … 0')])) + '\n')
    L.append(dg(t['sig_cap'], bf([(8, 'k1', 'word', t['sig_w'], '8 B'), (8, 'k3', 'incarnation', t['sig_i'], '8 B'), (16, 'k5', 'type key', t['sig_k'], '16 B')])) + '\n')
    L.append(f'## {t["str_h"]}\n\n{t["str_p"]}\n')
    L.append(dg(t['str_cap'],
                bf([(24, 'k1', t['hdr'], '24 B'), (8, 'k2', 'buf', t['buf_s']), (4, 'k2', 'len', t['len_s']), (4, 'k2', 'units', t['units_s']),
                    (4, 'k2', 'start', t['start_s']), (4, 'k2', 'cap', t['cap_s']), (4, 'k2', 'used', t['used_s']), (4, 'dim', t['pad'], ''), (40, 'k6', t['text'], '40 B ' + t['texts'])], ['0', '24', '32', '36', '40', '44', '48', '52', '56'], '96'),
                '', note=t['str_note']) + '\n')
    return '\n'.join(L)


def moving_page(t):
    L = [f'# {t["mv_title"]}\n\n{t["mv_intro"]}\n', f'## {t["mv_see_h"]}\n\n{t["mv_code"]}\n\n{t["mv_see_p"]}\n',
         f'## {t["mv_life_h"]}\n']
    L.append(dg(t['mv_life_cap'], flow([
        (t['s1'], 'k5', '0x7f3a_1c40_1040', t['s1t']),
        (t['s2'], 'k2', '0x7f3a_1c48_8a10', t['s2t']),
        (t['s3'], 'k3', '0x7f3a_1c5c_c230', t['s3t']),
        (t['s4'], 'k1', '0x4008_0004_2a18', t['s4t']),
    ])) + '\n')
    L.append(f'## {t["mv_when_h"]}\n\n{t["mv_when_tbl"]}\n')
    L.append(f'## {t["mv_why_h"]}\n\n{t["mv_why"]}\n')
    L.append(f'## {t["mv_fix_h"]}\n\n{t["mv_fix_p"]}\n')
    L.append(dg(t['fw_cap'], bf([(4, 'k7', 'mark', t['fw_mark'], 'bit 31'), (4, 'k2', 'info', '', ''), (8, 'k3', 'visit', '', ''), (8, 'k1', 'drop', '→ ' + t['fw_drop'], ''), (20, 'dim', 'T', t['fw_rest'])])) + '\n')
    L.append(f'{t["mv_fix_list"]}\n')
    L.append(f'## {t["mv_keep_h"]}\n\n{t["mv_keep_tbl"]}\n')
    L.append(f'## {t["mv_break_h"]}\n\n{t["mv_break_tbl"]}\n')
    L.append(f'## {t["mv_j8_h"]}\n\n{t["mv_j8"]}\n')
    return '\n'.join(L)


for lang, t in T.items():
    (ROOT / lang / 'src' / 'bits.md').write_text(bits_page(t))
    (ROOT / lang / 'src' / 'moving.md').write_text(moving_page(t))
print('ok')
