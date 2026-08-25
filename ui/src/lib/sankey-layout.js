/**
 * 桑基布局（spec §11.6）。
 *
 * **d3 只做布局计算，绝不碰 DOM** —— SVG 全部由 Svelte 的 {#each} 渲染。
 * 不使用 d3 的 enter/exit/update：那会与 Svelte 的响应式打架，两套东西
 * 争同一批 DOM 节点。
 *
 * 以下五条是实测踩出来的，改动前请先复现：
 *   1. sankey() 会**就地改写**输入对象，还会塞进循环引用 → 必须深拷贝
 *   2. nodeSort(null) + linkSort(null) 才能让列内顺序 = 输入数组顺序
 *   3. 零流带 / 全零权重时 d3 静默产出 NaN → 必须在调用前挡住
 *   4. extent 用满整幅 SVG 会让两侧标签被裁掉 → 留 gutter
 *   5. 渐变 id 含 % 时 url(#…) 静默不上色 → 不能用 encodeURIComponent
 *
 * 这里只算几何，**不解释流带粗细的含义** —— 含义由行上的 `weightUnit`
 * 决定，措辞统一走 `weight.js`。本模块刻意不出现「字节」二字。
 */
import { sankey, sankeyLinkHorizontal } from 'd3-sankey';

export const NODE_W = 12;
export const NODE_PAD = 10;

/** 标签留白。数值取自 mockup：960 宽下左列在 x=130、右列在 x=730。 */
export const GUTTER_L = 130;
export const GUTTER_R = 218;

const linkPath = sankeyLinkHorizontal();

/**
 * SVG 渐变 id。出站名可能是中文，而 `encodeURIComponent` 产出的 `%xx`
 * 放进 `url(#…)` 会**静默失效**（元素完全不上色，且 getComputedStyle
 * 看起来还是对的，极难排查）。逐字符转 base36 既避开 %，也保证
 * 首字符是字母、结果稳定可逆推。
 */
export function gradientId(name) {
  return 'g' + [...String(name)].map((c) => c.codePointAt(0).toString(36)).join('-');
}

/**
 * 一行画图用的量。
 *
 * 优先 `weight`（flows.js / aggregate.js 的契约），没有才退回 `bytes`。
 * 这不是防御性写法：当前后端不上报逐流字节，`bytes` 恒为 0，按它画图
 * 会得到一张永远空着的图 —— 而数据其实是有的，只是量是连接数。
 * 阶段 2 补上字节后 `weight` 自动变成字节数，这里一个字都不用改。
 */
function weightOf(r) {
  return typeof r.weight === 'number' ? r.weight : (r.bytes ?? 0);
}

/**
 * 流水行 → 三层图。返回 null 表示数据不足以画图（调用方走空状态）。
 *
 * 流带必须按 (source,target) 聚合：同一条规则会被多个站点命中，
 * 因此 rule→outbound 这条边在原始行里重复出现。不聚合的话
 * d3 会画出两条叠在一起的带子，Svelte 的 keyed each 更会直接抛
 * each_key_duplicate 让整个组件渲染不出来（实测）。
 */
export function toGraph(rows) {
  if (!rows?.length) return null;

  const seen = new Set();
  const nodes = [];
  const agg = new Map();

  const addNode = (id, label, layer, dest) => {
    if (seen.has(id)) return;
    seen.add(id);
    nodes.push({ id, label, layer, dest });
  };
  const addEdge = (source, target, value, dest) => {
    const key = `${source}>${target}`;
    const cur = agg.get(key);
    if (cur) cur.value += value;
    else agg.set(key, { key, source, target, value, dest });
  };

  for (const r of rows) {
    // 零权重的流带会让 d3 的 ky 缩放系数变成 Infinity，全图 NaN
    const w = weightOf(r);
    if (!(w > 0)) continue;
    const s = `s:${r.site}`;
    const u = `r:${r.rule}`;
    const o = `o:${r.outbound}`;
    // 左层与中层节点保持中性灰，只有出站节点满色 → 只有它带 dest
    addNode(s, r.site, 0, null);
    addNode(u, r.rule, 1, null);
    addNode(o, r.outbound, 2, r.outbound);
    addEdge(s, u, w, r.outbound);
    addEdge(u, o, w, r.outbound);
  }

  const links = [...agg.values()];
  return links.length ? { nodes, links } : null;
}

/**
 * 计算布局。`graph.nodes` 的数组顺序**就是**最终的列内顺序 ——
 * 调用方通过重排该数组来实现「节点顺序一旦确定即冻结」。
 */
export function layout(graph, width, height) {
  const gen = sankey()
    .nodeId((d) => d.id)
    // 层号由我们显式给定，不让 d3 从图结构推导 —— 推导出的 depth
    // 在出现跨层边时会漂移
    .nodeAlign((d) => d.layer)
    .nodeSort(null)   // ← 冻结列内顺序 = 输入数组顺序
    .linkSort(null)
    .nodeWidth(NODE_W)
    .nodePadding(NODE_PAD)
    .extent([[GUTTER_L, 12], [width - GUTTER_R + NODE_W, height - 12]]);

  // structuredClone：d3 会往节点上塞 sourceLinks/targetLinks（循环引用），
  // 并把 link.source 从字符串替换成节点对象。复用同一份对象跑第二次
  // 布局会读到脏数据。
  const out = gen(structuredClone(graph));

  return {
    // 只带出渲染要用的字段：d3 塞进去的 sourceLinks/targetLinks 是循环引用，
    // 原样交给 Svelte 的响应式代理会让它去遍历整张图
    nodes: out.nodes.map((n) => ({
      id: n.id,
      label: n.label,
      layer: n.layer,
      dest: n.dest,
      value: n.value,
      x0: n.x0,
      x1: n.x1,
      y0: n.y0,
      y1: n.y1,
    })),
    links: out.links.map((l) => ({
      key: l.key,
      d: linkPath(l),
      width: l.width,
      value: l.value,
      dest: l.dest,
      sourceId: l.source.id,
      targetId: l.target.id,
    })),
    // 全局横向渐变的端点：第一列右缘 → 末列左缘。
    // 用 userSpaceOnUse 而非默认的 objectBoundingBox，才能让**整条路径
    // 共享同一个渐变**，视觉语义即「未分类的流量被逐层筛清」。
    gradientX: [
      Math.min(...out.nodes.map((n) => n.x1)),
      Math.max(...out.nodes.map((n) => n.x0)),
    ],
  };
}

/** 按冻结的顺序重排节点数组；新节点追加到末尾（顺序因此只增不改）。 */
export function applyFrozenOrder(graph, frozenOrder) {
  if (!frozenOrder?.length) return graph;
  const rank = new Map(frozenOrder.map((id, i) => [id, i]));
  return {
    nodes: [...graph.nodes].sort(
      (a, b) => (rank.get(a.id) ?? Number.MAX_SAFE_INTEGER) - (rank.get(b.id) ?? Number.MAX_SAFE_INTEGER)
    ),
    links: graph.links,
  };
}
