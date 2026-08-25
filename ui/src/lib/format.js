/** 展示层格式化。集中一处并单测，避免各视图各写一份、格式还不一致。 */

/** 「无数据」的统一占位。用全角破折号而非 "N/A"：它在等宽列里占一格、不抢眼，
 * 且与真实数字有明显的字形差异，扫读时不会被误认成某个值。 */
const DASH = '—';

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB', 'PB', 'EB'];

/**
 * 二进制进位的字节数，保持三位有效数字。
 * 三位有效数字是为了让数字**列宽稳定** —— 变宽的数字列没法纵向扫读。
 */
export function bytes(n) {
  if (typeof n !== 'number' || !Number.isFinite(n) || n < 0) return DASH;
  if (n < 1024) return `${Math.round(n)} B`;
  let v = n;
  let i = 0;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i++;
  }
  // 三位有效数字：<10 保留两位小数，<100 保留一位，其余取整
  const s = v < 10 ? v.toFixed(2) : v < 100 ? v.toFixed(1) : String(Math.round(v));
  return `${s} ${UNITS[i]}`;
}

/** 计数（连接数、命中数）。加千分位，量级要能一眼看出来。 */
export function count(n) {
  if (typeof n !== 'number' || !Number.isFinite(n) || n < 0) return DASH;
  return Math.round(n).toLocaleString('en-US');
}

/** ratio 是 0..1 的比值。极小但非零的占比显示为 `<1%` 而非 `0%` ——
 * 后者会让用户以为这条流没有流量，而它其实只是很小。 */
export function pct(ratio) {
  if (typeof ratio !== 'number' || !Number.isFinite(ratio)) return DASH;
  if (ratio > 0 && ratio < 0.005) return '<1%';
  return `${Math.round(ratio * 100)}%`;
}

/** 延迟。未测得时给占位符 —— 显示 0 ms 会被误读成「极快」。 */
export function ms(v) {
  if (typeof v !== 'number' || !Number.isFinite(v) || v < 0) return DASH;
  if (v < 1000) return `${Math.round(v)} ms`;
  return `${(v / 1000).toFixed(2)} s`;
}
