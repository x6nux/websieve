/**
 * Top N 聚合（spec §11.6）。
 *
 * 目标站点 Top 12、命中规则 Top 8，超出的压成「其他 N 个」。
 * **出站永不折叠** —— 它是颜色语义的载体，折叠掉就等于把「去哪」这件事
 * 藏起来，而那正是整个视图存在的理由。
 *
 * 与 flows.js 的契约：每行带 `weight` 与 `weightUnit`（见那边的说明）。
 * 折叠**按 weight 排名、按 weight 求和、原样透传 weightUnit** ——
 * 三者缺一，前面守住的「不拿连接数假装字节数」就会在这一步漏掉。
 */

import { WEIGHT_BYTES, WEIGHT_CONNS } from './flows.js';

export const OTHER_SITE = '其他';
export const OTHER_RULE = '其他';

export const DEFAULT_LIMITS = { sites: 12, rules: 8 };

/** 与 flows.js 用同一个分隔符：域名与规则值里都不可能出现 NUL，
 * 而 '|' 之类的可见字符有碰撞风险（规则值可以含任意字符）。 */
const SEP = '\u0000';

/**
 * 取值最大的前 N 个键。
 * 并列时按键名排序作为 tiebreak —— 否则结果会随 Map 插入顺序抖动，
 * 而抖动意味着节点顺序每秒都在变，正是 §11.6 要避免的。
 */
export function topN(totals, n) {
  return new Set(
    [...totals.entries()]
      .sort((a, b) => b[1] - a[1] || String(a[0]).localeCompare(String(b[0])))
      .slice(0, Math.max(0, n))
      .map(([k]) => k)
  );
}

/**
 * 一行的排序/求和权重。
 *
 * 优先用 flows.js 给的 `weight`，没有才退回 `bytes`。这不是防御性写法，
 * 而是正确性要求：无字节数据时 bytes 恒为 0，按它排名等于全部并列，
 * 留下谁全看键名字典序 —— 图上会随机砍掉连接数最多的站点，
 * 而用户完全看不出为什么。排名量必须与画图量是同一个量。
 */
function weightOf(r) {
  return typeof r.weight === 'number' ? r.weight : (r.bytes ?? 0);
}

/**
 * 合并两行的单位。两边一致就沿用；不一致返回 null。
 *
 * 不一致本身就是 bug（同一张图里混了两种来源的数据）。返回 null 让它显形 ——
 * 渲染层拿到 null 就无从标注轴，只能报错或退化，这正是想要的；
 * 若在这里挑一个「看起来对」的单位，错误就被永久藏起来了。
 */
function mergeUnit(a, b) {
  if (a === undefined) return b;
  if (b === undefined) return a;
  return a === b ? a : null;
}

function sumBy(rows, key) {
  const m = new Map();
  for (const r of rows) m.set(r[key], (m.get(r[key]) ?? 0) + weightOf(r));
  return m;
}

/**
 * 原始流水 → 折叠后的流水。字节、连接数与 weight 三者都守恒。
 * 折叠后可能出现重复的 (站点,规则,出站) 三元组，必须合并 —— 否则
 * 桑基图会画出两条叠在一起的带子，Svelte 的 keyed each 还会直接报错
 * （each_key_duplicate，且整个组件渲染不出来）。
 */
export function prepareFlows(rows, limits = DEFAULT_LIMITS) {
  if (!rows?.length) return [];

  const keepSites = topN(sumBy(rows, 'site'), limits.sites);
  const keepRules = topN(sumBy(rows, 'rule'), limits.rules);

  const droppedSites = new Set();
  const droppedRules = new Set();
  for (const r of rows) {
    if (!keepSites.has(r.site)) droppedSites.add(r.site);
    if (!keepRules.has(r.rule)) droppedRules.add(r.rule);
  }
  const siteLabel = `${OTHER_SITE} ${droppedSites.size} 个站点`;
  const ruleLabel = `${OTHER_RULE} ${droppedRules.size} 条规则`;

  const merged = new Map();
  for (const r of rows) {
    const site = keepSites.has(r.site) ? r.site : siteLabel;
    const rule = keepRules.has(r.rule) ? r.rule : ruleLabel;
    const k = [site, rule, r.outbound].join(SEP);
    const cur = merged.get(k);
    if (cur) {
      cur.bytes += r.bytes ?? 0;
      cur.conns += r.conns ?? 0;
      cur.weight += weightOf(r);
      cur.weightUnit = mergeUnit(cur.weightUnit, r.weightUnit);
    } else {
      merged.set(k, {
        site,
        rule,
        outbound: r.outbound,
        bytes: r.bytes ?? 0,
        conns: r.conns ?? 0,
        weight: weightOf(r),
        weightUnit: r.weightUnit,
        // 聚合行用最暗的中性色，且不参与「点击跳转到规则视图」
        aggregated: site === siteLabel || rule === ruleLabel,
      });
    }
  }
  return [...merged.values()];
}

export { WEIGHT_BYTES, WEIGHT_CONNS };
