/**
 * 权重的措辞层 —— 「粗细代表什么」这件事在 UI 上的唯一词源。
 *
 * 背景见 `flows.js`：后端的 `ConnectionDelta` 目前没有字节数，所以桑基图的
 * 流带宽度、表格的数值列画的都是**连接数**，不是吞吐量。这个事实必须出现在
 * 用户看得见的地方，而不是只写在注释里。
 *
 * 因此渲染层不许自己拼「字节」「MB」这类词：轴标签、图例、列头、屏幕阅读器
 * 摘要全部从这里取词，词从 `weightUnit` 推出。单位一变，所有措辞跟着变；
 * 单位说不清，就一个字也说不出来（返回 null / 冲突文案），而不是随便挑一个。
 */

import { WEIGHT_BYTES, WEIGHT_CONNS } from './flows.js';
import { bytes, count } from './format.js';

export { WEIGHT_BYTES, WEIGHT_CONNS };

/**
 * 单位说不清时的统一文案。
 *
 * 「说不清」只有一种成因：同一批行里混了两种来源的数据（`aggregate.js` 的
 * `mergeUnit` 在这种情况下返回 null）。这是 bug，不是可以糊过去的边界情况 ——
 * 一半字节一半连接数的图没有任何含义，所以渲染层必须停下来把它说出来。
 */
export const UNIT_CONFLICT_MSG =
  '流量数据的计量单位不一致：一部分按字节、一部分按连接数。两种量混在同一张图里没有意义，因此这里不作绘制。这属于数据来源冲突，请检查事件源。';

/**
 * 一行画图/排序用的量。
 *
 * 优先 `weight`（flows.js / aggregate.js 的契约），没有才退回 `bytes`。
 * 这不是防御性写法：当前后端不上报逐流字节，`bytes` 恒为 0，按它画图
 * 会得到一张永远空着的图 —— 而数据其实是有的，只是量是连接数。
 * 阶段 2 补上字节后 `weight` 自动变成字节数，调用方一个字都不用改。
 *
 * 定义只此一处：粗细取哪个字段与「粗细代表什么」是同一个决定，
 * 分成两处迟早会漂移成「按 A 排名、按 B 画图」。
 */
export function rowWeight(r) {
  return typeof r?.weight === 'number' ? r.weight : (r?.bytes ?? 0);
}

/** 一批行的权重合计。 */
export function totalWeight(rows) {
  return (rows ?? []).reduce((s, r) => s + rowWeight(r), 0);
}

/** 量的名词。单位未知时返回 null —— 没有词可用，调用方只能走冲突分支。 */
export function unitNoun(unit) {
  if (unit === WEIGHT_BYTES) return '字节';
  if (unit === WEIGHT_CONNS) return '连接数';
  return null;
}

/**
 * 「流带粗细代表什么」的一句话说明，直接贴在图上。
 *
 * 连接数那一支刻意把**否定**也写出来（「不是吞吐量」）：只说「粗细 = 连接数」
 * 仍然会被读成吞吐量的近似，因为桑基图这个形态本身在暗示流量。
 */
export function widthLegend(unit) {
  if (unit === WEIGHT_BYTES) return '流带粗细 = 字节数';
  if (unit === WEIGHT_CONNS) {
    return '流带粗细 = 连接数，不是吞吐量 —— 后端尚未上报逐流字节';
  }
  return null;
}

/** 单个权重值的显示。单位未知时给占位符，绝不退回某个「看起来对」的单位。 */
export function formatWeight(v, unit) {
  if (unit === WEIGHT_BYTES) return bytes(v);
  if (unit === WEIGHT_CONNS) return count(v);
  return '—';
}

/** 合计的读数，带量词。用于工具栏与屏幕阅读器摘要。 */
export function weightSummary(v, unit) {
  if (unit === WEIGHT_BYTES) return bytes(v);
  if (unit === WEIGHT_CONNS) return `${count(v)} 条连接`;
  return '—';
}

/**
 * 一批行共同的单位。冲突或无从判定时返回 null。
 *
 * 「没有任何一行声明单位」同样返回 null，而不是猜成字节：不知道粗细代表什么
 * 就是不能画，这比画出一张标错轴的图好。应用里的行全部来自 `FlowStore.rows()`
 * 与 `prepareFlows()`，两者都必然带 `weightUnit`，所以这条路径只会在有人绕过
 * 数据层直接塞数据时触发 —— 那正是需要被拦住的时候。
 */
export function rowsUnit(rows) {
  if (!rows?.length) return null;
  let unit;
  for (const r of rows) {
    const u = r?.weightUnit;
    if (u === undefined || u === null) continue;
    if (unit === undefined) unit = u;
    else if (unit !== u) return null;
  }
  return unit ?? null;
}
