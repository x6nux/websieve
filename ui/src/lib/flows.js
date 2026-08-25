/**
 * connection 事件 → 逐流明细。
 *
 * 存在的理由见「阶段 4 交接假设」：阶段 4 的 traffic 事件只有总量
 * （up_bytes / down_bytes / active），没有分流；桑基图与表视图需要的
 * 逐流明细只能从 connection 事件（ConnectionDelta）在前端聚合出来。
 *
 * **诚实优先。** ConnectionDelta 目前的字段只有 { id, target, outbound, state } ——
 * 没有字节数。因此「流带有多粗」这件事现在画的是**连接数**，不是吞吐量。
 *
 * 这个事实不能只写在注释里。注释拦不住下游把一个叫 `bytes` 或 `value` 的
 * 数字当字节渲染，然后在轴上标个 "MB" —— 那就是拿连接数假装成吞吐量，
 * 是这个项目反复在别的形态下抓到的同一个错误。
 *
 * 所以单位是**跟着数值一起走的**：每一行都带 `weight` 与 `weightUnit`，
 * 拿到前者就必然拿到后者。渲染层要标轴、要写图例，只能从 `weightUnit`
 * 取措辞，没有第二个来源可以糊弄过去。
 *
 * 等阶段 2 给事件补上 bytes（以及 rule）字段，这里会自动开始用真实值，
 * `weightUnit` 随之翻成 bytes，调用方无需改动。
 */

/** 权重的两种含义。字符串常量而非 boolean —— `weightUnit === 'conns'`
 * 在调用点读得出意思，`byteMode === false` 读不出。 */
export const WEIGHT_BYTES = 'bytes';
export const WEIGHT_CONNS = 'conns';

/** 聚合键的分隔符。域名与规则值里都不可能出现 NUL，而 '|' 之类的可见字符
 * 有碰撞风险（规则值可以含任意字符）。**不能省** —— 直接拼接会让
 * ('ab','c') 与 ('a','bc') 撞成同一条流。 */
const SEP = '\u0000';

/** 从 "host:port" 取出 host。IPv6 字面量形如 "[::1]:443"，不能按最后一个冒号切。 */
export function hostOf(target) {
  if (!target) return '';
  const s = String(target);
  if (s.startsWith('[')) {
    const end = s.indexOf(']');
    return end > 0 ? s.slice(1, end) : s;
  }
  const i = s.lastIndexOf(':');
  // 冒号后面必须全是数字才算端口，否则原样返回
  return i > 0 && /^\d+$/.test(s.slice(i + 1)) ? s.slice(0, i) : s;
}

/** 规则未知时的占位。桑基图的中间层不能有空节点。 */
export const UNKNOWN_RULE = '（规则未记录）';

export class FlowStore {
  /**
   * @param {number} cap 最多保留多少条流。默认 500 —— 远超 Top 12 的需要，
   *   但足够小到不会在长时间运行后吃掉内存。
   */
  constructor(cap = 500) {
    this.cap = cap;
    this.flows = new Map();
    this.seen = new Set(); // 去重：同一连接 id 只计一次
    this.bytesSeen = false;
  }

  /** 事件里是否真的带了字节数。 */
  hasBytes() {
    return this.bytesSeen;
  }

  /**
   * 当前的权重含义。渲染层的轴标签、图例、tooltip 措辞全部由它决定 ——
   * 这是「不拿连接数假装字节数」在代码里的落点。
   */
  weightUnit() {
    return this.bytesSeen ? WEIGHT_BYTES : WEIGHT_CONNS;
  }

  apply(deltas = []) {
    for (const d of deltas) {
      if (!d) continue;
      if (typeof d.bytes === 'number' && d.bytes > 0) this.bytesSeen = true;

      // close 只是状态变更，不产生新连接计数
      const isNew = d.state !== 'close' && !this.seen.has(d.id);
      if (d.state !== 'close') this.seen.add(d.id);

      const site = hostOf(d.target);
      const rule = d.rule || UNKNOWN_RULE;
      const outbound = d.outbound || 'DIRECT';
      // 去向是流的身份：同一站点走不同出站是两条不同的流。
      const key = [site, rule, outbound].join(SEP);

      const cur = this.flows.get(key);
      if (cur) {
        if (isNew) cur.conns += 1;
        cur.bytes += d.bytes ?? 0;
      } else {
        this.flows.set(key, {
          site,
          rule,
          outbound,
          conns: isNew ? 1 : 0,
          bytes: d.bytes ?? 0,
        });
      }
    }
    this.#trim();
  }

  /** 一条流当前该按什么计。单位一翻转，**所有**行一起翻 ——
   * 否则图上会一半是字节、一半是连接数，而那种图没有任何含义。 */
  #weightOf(f) {
    return this.bytesSeen ? f.bytes : f.conns;
  }

  /**
   * 触顶时淘汰**最小**的流而非最旧的。
   * 淘汰最旧会让长期活跃的大流被新来的一次性小流挤掉，
   * 而那恰恰是用户最想看到的东西。
   */
  #trim() {
    if (this.flows.size <= this.cap) return;
    const sorted = [...this.flows.entries()].sort(
      (a, b) => this.#weightOf(b[1]) - this.#weightOf(a[1]) || b[1].conns - a[1].conns
    );
    this.flows = new Map(sorted.slice(0, this.cap));

    // ponytail: seen 集合触顶后整个清空，而不是按 LRU 逐出。
    // 上限：清空后若某个**旧** id 再次以 open 事件到达，会被重复计一次连接。
    // 可接受的理由：连接 id 在后端是单调递增的 u64（events.rs），旧 id 不会
    // 重放，唯一的触发路径是事件总线重复投递同一条 open —— 而阶段 4 的
    // ConnectionQueue 是 drain 语义，不重投。
    // 升级路径：真需要精确去重时把 seen 换成保序的 Map 做 LRU 逐出，
    // 或等后端在 close 事件里带上「该连接已结束」并据此主动移除 id。
    if (this.seen.size > this.cap * 20) this.seen = new Set();
  }

  /**
   * 逐流明细。每行的 `weight` 是画图用的粗细来源，`weightUnit` 是它的含义 ——
   * 两者成对出现，渲染层拿不到一个而漏掉另一个。
   */
  rows() {
    const unit = this.weightUnit();
    return [...this.flows.values()].map((f) => ({
      ...f,
      weight: this.#weightOf(f),
      weightUnit: unit,
    }));
  }

  reset() {
    this.flows.clear();
    this.seen.clear();
    // 单位也要退回连接数：换配置/重连后若留着旧的 true，
    // 就会拿「上次见过字节数」的结论去画这次全是 0 的字节数。
    this.bytesSeen = false;
  }
}
