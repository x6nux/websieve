/**
 * 规则排序的纯逻辑。
 *
 * 规则顺序**就是**语义（首命中即返回），所以排序是核心操作而非便利功能。
 *
 * 抽成纯函数是为了能单测：HTML5 drag 事件在 jsdom 下不可靠，
 * 但「把第 i 项移到第 j 位」是可穷举的。组件里只留事件接线。
 */

/** 把 from 位置的元素移到 to 位置。越界或原地不动时返回原数组引用。 */
export function move(list, from, to) {
  if (
    from === to ||
    !Number.isInteger(from) ||
    !Number.isInteger(to) ||
    from < 0 ||
    to < 0 ||
    from >= list.length ||
    to >= list.length
  ) {
    return list;
  }
  const out = [...list];
  const [item] = out.splice(from, 1);
  out.splice(to, 0, item);
  return out;
}

/**
 * 键盘排序：Alt+↑ / Alt+↓。
 *
 * **拖拽必须有键盘等价物**，否则这个核心操作对键盘用户等于不存在 ——
 * 这是无障碍硬要求，不是加分项。
 *
 * 返回新的 index 让调用方把焦点跟到移动后的位置 —— 焦点丢失是
 * 键盘操作最常见的断裂点：DOM 重排后原来那个按钮节点可能已被复用给别的行，
 * 焦点留在原地等于「移动完人就不知道自己在哪了」。
 */
export function keyboardMove(list, index, key) {
  if (key === 'ArrowUp') {
    return { list: move(list, index, index - 1), index: Math.max(0, index - 1) };
  }
  if (key === 'ArrowDown') {
    return {
      list: move(list, index, index + 1),
      index: Math.min(list.length - 1, index + 1),
    };
  }
  return { list, index };
}

/**
 * 位置播报文案。
 *
 * 拖拽的视觉反馈（行跟着鼠标走）在键盘路径上一个字都不存在，
 * 屏幕阅读器用户按完 Alt+↓ 得到的是**彻底的静默**。所以位置必须
 * 显式播报，而不是「靠视觉隐含」—— 后者对辅助技术等于没发生。
 *
 * 说明「首命中即返回」是因为这里的序号不是装饰：第 3 条与第 4 条的
 * 差别可能就是流量走没走代理。只报「已移动」而不报名次等于没报。
 */
export function positionAnnouncement(index, total, label) {
  if (!Number.isInteger(index) || !Number.isInteger(total) || total <= 0) return '';
  if (index < 0 || index >= total) return '';
  const name = label ? `${label} ` : '';
  const rank = `${name}现在是第 ${index + 1} 条，共 ${total} 条`;
  if (index === 0) return `${rank}，位于最前，将被最先匹配。`;
  if (index === total - 1) return `${rank}，位于最后。`;
  return `${rank}。规则按顺序匹配，首命中即返回。`;
}
