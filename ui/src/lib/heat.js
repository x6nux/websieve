/**
 * 命中热度 → 行背景不透明度（signature ①，spec §11.5）。
 *
 * 用**中性色**而非出站色染色：出站色码在整个产品里只表达一件事（去哪），
 * 拿它来表达热度会让两种语义打架。
 *
 * 刻度必须是**对数**的。实测命中数跨三个数量级（mockup 里 211 → 88,120），
 * 线性归一化下 `211/88120 = 0.24%`，乘以上限得 `.000124` —— 肉眼完全不可见，
 * 除榜首外全部规则染色一律等于零，signature 也就废了。
 * 而「哪几条规则是死的」正是这个 signature 唯一要回答的问题。
 */

/** 上限取自 §11.4 的 `rgba(255,255,255,.014 ~ .052)` 区间上端；再高会盖过探针的命中高亮 */
export const HEAT_MAX = 0.052;

/**
 * 区间下端。**非零命中一律不低于它**，这一条是刻意的：
 *
 * 纯对数归一化下 `log(1)/log(max) = 0`，于是「命中过 1 次」与「一次都没命中」
 * 渲染成同一个透明。但这两件事恰恰是这个 signature 要分开的 ——
 * 命中过一次的规则是活的（只是冷），一次没命中过的才是死的。
 * 把非零命中抬进 [HEAT_MIN, HEAT_MAX]，透明就重新只表示一件事：**从未命中**。
 *
 * 数值与 tokens.css 的 `--heat-min` 同源（§11.4 定死的区间下端）。
 */
export const HEAT_MIN = 0.014;

/**
 * 归一化到 [HEAT_MIN, HEAT_MAX]；零命中（或非法输入）返回 0。
 *
 * 用 `log1p` 而非 `log`：`log(1)` 为 0 会把「命中 1 次」压回死规则那一档，
 * `log1p` 则让 hits=1 落在区间下端而不是零。
 */
export function heat(hits, maxHits) {
  if (typeof hits !== 'number' || !Number.isFinite(hits) || hits <= 0) return 0;
  if (typeof maxHits !== 'number' || !Number.isFinite(maxHits) || maxHits <= 0) return 0;
  // 全表只有一条规则（或全部同值）时，它就是最热的
  if (maxHits <= 1) return HEAT_MAX;
  const t = Math.min(1, Math.max(0, Math.log1p(hits) / Math.log1p(maxHits)));
  return HEAT_MIN + t * (HEAT_MAX - HEAT_MIN);
}

/**
 * 直接给出可用的 CSS 值，避免各处重复拼字符串。
 *
 * 零命中返回 `transparent` 而不是 `rgba(...,0)`：两者渲染一致，
 * 但前者在 devtools 里一眼看得出「这行是死的」，后者得去数小数点。
 */
export function heatColor(hits, maxHits) {
  const a = heat(hits, maxHits);
  return a > 0 ? `rgba(255,255,255,${a.toFixed(4)})` : 'transparent';
}
