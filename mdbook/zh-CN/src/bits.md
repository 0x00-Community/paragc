# 比特布局

paragc 和 j8 放进内存的每个字，必要处按比例画出。宽度和位数或字节数成比例；单个位的标志画宽一些，好放得下标签。

## 私有对象

一个对象由 24 字节的头、数据和可选的尾巴组成，向上取整到 8 字节：`(HEADER + size_of::<T>() + tail).next_multiple_of(8)`。`Gc` 指向数据，不指向头。

<div class="dg"><div class="dg-cap">一个私有对象的内存</div><div class="bf ruled"><div class="k1" style="--w:4"><b>mark</b><small>4 B</small><u>0</u></div><div class="k2" style="--w:4"><b>info</b><small>4 B</small><u>4</u></div><div class="k3" style="--w:8"><b>visit</b><small>追踪 / 改写 / 询问</small><u>8</u></div><div class="k4" style="--w:8"><b>drop</b><small>析构，搬走后存新地址</small><u>16</u></div><div class="k5" style="--w:14"><b>数据 <code>T</code></b><small>size_of::&lt;T&gt;()</small><u>24 ▲ Gc</u></div><div class="k6 dim" style="--w:8"><b>尾巴</b><small>纯字节，可选</small><u>24 + T</u><u class="end">end</u></div></div><div class="dg-note"><code>Gc</code> 指在这里 ↑ 偏移 24</div></div>

<div class="dg"><div class="dg-cap"><code>mark</code> · u32 · 第 31 位在最左</div><div class="bf"><div class="k7" style="--w:3"><b>FORWARDED</b><small>已搬走</small><i>31</i></div><div class="k3" style="--w:3"><b>BUSY</b><small>正在复制</small><i>30</i></div><div class="k1" style="--w:26"><b>epoch</b><small>最后一次标记的纪元 · 0 = 年轻</small><i>29 … 0</i></div></div><div class="dg-note">FORWARDED 和 BUSY 只在会搬对象的回收期间出现。纪元永远到不了它们：MAX_EPOCH = 2³⁰ − 1。</div></div>

<div class="dg"><div class="dg-cap"><code>info</code> · u32</div><div class="bf"><div class="k7" style="--w:3"><b>L</b><small>大对象</small><i>31</i></div><div class="k3" style="--w:3"><b>P</b><small>计数奇偶</small><i>30</i></div><div class="k4" style="--w:6"><b>RC</b><small>引用计数 · 到 63 粘住</small><i>29 … 24</i></div><div class="k2" style="--w:20"><b>granules</b><small>粒数 · × 8 B，含头</small><i>23 … 0</i></div></div></div>

## 跨堆对象

头是 16 字节，不存代码指针：别的进程不链接这个类型也能读、能回收这个对象。`mark` 和 `info` 是同样的两个字，所以块、行、清扫和私有堆共用代码。

<div class="dg"><div class="dg-cap">一个跨堆对象的内存</div><div class="bf ruled"><div class="k1" style="--w:4"><b>mark</b><small>4 B</small><u>0</u></div><div class="k2" style="--w:4"><b>info</b><small>4 B</small><u>4</u></div><div class="k3" style="--w:4"><b>ty</b><small>类型表编号</small><u>8</u></div><div class="k4" style="--w:4"><b>stamp</b><small>出生戳</small><u>12</u></div><div class="k5" style="--w:14"><b>数据 <code>T</code></b><small>size_of::&lt;T&gt;()</small><u>16</u></div><div class="k6 dim" style="--w:8"><b>尾巴</b><small>纯字节，可选</small><u>16 + T</u><u class="end">end</u></div></div></div>

## 晋升时换头

私有对象搬进跨堆时，24 字节的头换成 16 字节的头，数据和尾巴逐字节拷过去。

<div class="dg"><div class="dg-cap"></div><div class="swap"><span>私有</span><div class="bf"><div class="k1" style="--w:24"><b>Header</b><small>24 B</small></div><div class="k5" style="--w:32"><b>T</b></div><div class="k6" style="--w:16"><b>尾巴</b></div></div><span>跨堆</span><div class="bf"><div class="k3" style="--w:16"><b>PHeader</b><small>16 B</small></div><div class="k5" style="--w:32"><b>T</b></div><div class="k6" style="--w:16"><b>尾巴</b></div><div class="dim" style="--w:8"><b></b></div></div></div></div>

## 块与它的元数据

每个 32 KiB 的块有 128 行，每行 256 字节。每行一个 `u32` 标记：最后一次在这一行里发现活对象的回收的纪元。分配时在连续的空闲行里递增推进。

<div class="dg"><div class="dg-cap">一个块：128 行，每排 16 行</div><div class="blk"><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="old"></span><span class="old"></span><span class="old"></span><span class="old"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="old"></span><span class="old"></span><span class="old"></span><span class="old"></span><span class="old"></span><span class="old"></span><span class="old"></span><span class="old"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="old"></span><span class="old"></span><span class="old"></span><span class="old"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="free"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span><span class="live"></span></div><div class="legend"><span class="live">带上一次完成的纪元（活）</span><span class="old">标记更旧：重新空闲</span><span class="free">从没用过</span></div></div>

## 放大一行

一行是 32 个 8 字节的粒。**起点位图**每个粒一位，在每个对象的第一个粒置位；清扫和跨堆查找都靠走它。每个块的行标记 512 字节，起点位也是 512 字节。

<div class="dg"><div class="dg-cap">一行 256 字节：对象 A、B、C（6、3、12 个粒）和两个空洞</div><div class="bf exact ruled"><div class="k1" style="--w:6"><b>A</b><small>48 B</small><u>0</u></div><div class="k2" style="--w:3"><b>B</b><small>24 B</small><u>48</u></div><div class="dim" style="--w:5"><b>空洞</b><small>40 B</small><u>72</u></div><div class="k3" style="--w:12"><b>C</b><small>96 B</small><u>112</u></div><div class="dim" style="--w:6"><b>空洞</b><small>48 B</small><u>208</u><u class="end">256 B</u></div></div><div class="dg-sub">起点位图：每个粒一位，对象开始的地方置 1</div><div class="gran bits"><span class="on">1</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="on">1</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="on">1</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span><span class="">0</span></div></div>

## 槽与字

<div class="dg"><div class="dg-cap"><code>Slot&lt;'gc, T&gt;</code> · 两个字</div><div class="bf"><div class="k1" style="--w:1"><b>值</b><small>当前指针</small><i>8 B</i></div><div class="k2" style="--w:1"><b>上次计数的值</b><small>引用计数用</small><i>8 B</i></div></div></div>

<div class="dg"><div class="dg-cap"><code>WordSlot</code> 的值字</div><div class="bf"><div class="k3" style="--w:16"><b>标签</b><small>≥ 0xFFF8 即指针</small><i>63 … 48</i></div><div class="k1" style="--w:48"><b>数据地址</b><small>低 48 位</small><i>47 … 0</i></div></div><div class="dg-note">小于 <code>0xFFF8 &lt;&lt; 48</code> 的字是普通数据。对象搬动时，指针字的标签原样保留。</div></div>

## j8 的 JS 值

JS 的 `Value` 是一个 NaN-boxing 的 64 位字。double 原样存放；其余的值高 12 位是 `0xFFF`，接下来 4 位是种类，低 48 位是载荷。

<div class="dg"><div class="dg-cap">一个 double</div><div class="bf"><div class="k7" style="--w:3"><b>S</b><small>符号</small><i>63</i></div><div class="k3" style="--w:11"><b>exp</b><small>指数</small><i>62 … 52</i></div><div class="k2" style="--w:50"><b>fraction</b><small>尾数</small><i>51 … 0</i></div></div></div>

<div class="dg"><div class="dg-cap">其余的值</div><div class="bf"><div class="k7" style="--w:12"><b>0xFFF</b><i>63 … 52</i></div><div class="k4" style="--w:4"><b>kind</b><small>种类 + 1</small><i>51 … 48</i></div><div class="k1" style="--w:48"><b>payload</b><small>地址、编号或一位</small><i>47 … 0</i></div></div></div>

<div class="dg"><div class="dg-cap">各种类的高 16 位（蓝色：指向堆）</div><div class="chips"><div class="chip"><b>0xFFF1</b><span>undefined</span><small>无载荷</small></div><div class="chip"><b>0xFFF2</b><span>null</span><small>无载荷</small></div><div class="chip"><b>0xFFF3</b><span>布尔</span><small>0 或 1</small></div><div class="chip"><b>0xFFF5</b><span>未初始化（TDZ）</span><small>无载荷</small></div><div class="chip"><b>0xFFF6</b><span>惰性值</span><small>编号</small></div><div class="chip ptr"><b>0xFFF8</b><span>字符串</span><small>地址</small></div><div class="chip ptr"><b>0xFFF9</b><span>symbol</span><small>地址</small></div><div class="chip ptr"><b>0xFFFA</b><span>bigint</span><small>地址</small></div><div class="chip ptr"><b>0xFFFB</b><span>对象</span><small>地址</small></div><div class="chip ptr"><b>0xFFFC</b><span>访问器对</span><small>地址</small></div><div class="chip ptr"><b>0xFFFD</b><span>上下文</span><small>地址</small></div></div><div class="dg-note">所有指向堆的种类都从 <code>0xFFF8</code> 开始，正好是 <code>WordSlot</code> 开始把字当成指针的位置。所以存在 <code>WordSlot</code>（j8 的 <code>ValueCell</code>）里的 JS 值就是一条边：回收器追踪它、给它计数、搬动它，种类原样保留。</div></div>

## 跨堆用的字

<div class="dg"><div class="dg-cap"><code>CrossHeapSlot</code> · u64</div><div class="bf"><div class="k1" style="--w:64"><b>offset</b><small>相对地址区起点的偏移 · 每个进程都一样</small><i>63 … 0</i></div></div></div>

<div class="dg"><div class="dg-cap"><code>CrossHeapWeak</code> · u64</div><div class="bf"><div class="k1" style="--w:37"><b>offset / 8</b><small>1 TiB 地址区：40 位 − 3</small><i>63 … 27</i></div><div class="k4" style="--w:27"><b>stamp</b><small>低 27 位</small><i>26 … 0</i></div></div></div>

<div class="dg"><div class="dg-cap"><code>Signature</code> · 32 字节</div><div class="bf"><div class="k1" style="--w:8"><b>word</b><small>弱引用字</small><i>8 B</i></div><div class="k3" style="--w:8"><b>incarnation</b><small>堆化身号</small><i>8 B</i></div><div class="k5" style="--w:16"><b>type key</b><small>类型键</small><i>16 B</i></div></div></div>

## 例子：一个 JS 字符串

`JsStr` 是 32 字节的字段，文本放在尾巴里。40 字节的 ASCII 字符串在私有堆里占 96 字节、12 个粒；晋升后占 88 字节。

<div class="dg"><div class="dg-cap">私有堆里的「…40 个 ASCII 字节…」</div><div class="bf ruled"><div class="k1" style="--w:24"><b>头</b><small>24 B</small><u>0</u></div><div class="k2" style="--w:8"><b>buf</b><small>空，或视图读的缓冲区</small><u>24</u></div><div class="k2" style="--w:4"><b>len</b><small>UTF-8 字节数</small><u>32</u></div><div class="k2" style="--w:4"><b>units</b><small>UTF-16 码元数</small><u>36</u></div><div class="k2" style="--w:4"><b>start</b><small>视图起点</small><u>40</u></div><div class="k2" style="--w:4"><b>cap</b><small>可续写的空间</small><u>44</u></div><div class="k2" style="--w:4"><b>used</b><small>已认领，原子</small><u>48</u></div><div class="dim" style="--w:4"><b>填充</b><u>52</u></div><div class="k6" style="--w:40"><b>文本</b><small>40 B UTF-8</small><u>56</u><u class="end">96</u></div></div><div class="dg-note">非 ASCII 字符串的尾巴后面，对齐到 2 字节，再接 UTF-16 码元。</div></div>
