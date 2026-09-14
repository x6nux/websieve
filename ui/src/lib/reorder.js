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

/**
 * 把一次排序翻译成 `config_save` 的 ops。
 *
 * ## 为什么必须由**行号**来表达，而不是「写回整份数组」
 *
 * `config_save` 是**定点改写**：`{ op:'replace-rule', line, expect, value }`
 * 只动那一行，用户手写的注释、缩进、行尾注释一个字节都不碰（§5.1 选 YAML
 * 的唯一理由就是能写注释）。行号来自 `config_get` 的 `rules[].line` ——
 * 它之所以要单独一列而不留在 `config` 里，是因为 `Spanned<T>` 的 JSON
 * 序列化**只吐出值**，行号会在 JSON 化的路上悄悄丢掉。
 *
 * **前端只要在任何一环把 `line` 丢了，保留注释的编辑就彻底做不到了。**
 * 所以这里在缺失时直接抛错而不是跳过：静默降级会让「排序像是生效了」，
 * 而配置文件纹丝不动。
 *
 * ## `expect` 从哪来
 *
 * `expect` 是**「你读到那一行时它是什么」**，只能取自 `config_get` 的
 * 那份快照（`rules[i].raw`），**不能拿本地乐观更新后的状态去凑**。
 * 文件若在读与写之间被改过（用户开了原文编辑器、外部工具改过），行号就是
 * 陈旧的，照着它改会改到**别的规则**头上且悄无声息。带上 expect 才能让
 * 服务端把这类失败从静默损坏变成一次明确的拒绝。
 *
 * 因此下面 `expect` 一律取 `rules[i]`（原顺序），`value` 取 `next[i]`（新顺序）：
 * 一次排序 = 把值在**这些既有的行**上重新排列。
 *
 * ponytail: 行尾注释跟着**行**走，不跟着规则走。
 *   上限：`- GEOSITE,cn,DIRECT  # 国内直连` 与下一条互换后，
 *         `# 国内直连` 会留在原行、贴到换过来的那条规则屁股后面。
 *   升级路径：给 `wsieve-config::edit` 加一个 `move_rule_line(src, from, to)`，
 *         整行（连同其行尾注释与紧贴上方的注释块）搬移，而不是只换值。
 *         届时这里改成发一条 `move-rule` op。
 *
 * @throws {Error} 规则行缺 `line` 或 `raw` 时抛出 —— 那意味着定点改写已不可能。
 */
export function reorderOps(rules, from, to) {
  const next = move(rules, from, to);
  if (next === rules) return [];

  const ops = [];
  for (let i = 0; i < rules.length; i++) {
    const at = rules[i]; // 这一行原本是什么 —— expect 的唯一来源
    const now = next[i]; // 这一行将要变成什么

    if (!Number.isInteger(at?.line) || at.line <= 0) {
      throw new Error(
        `第 ${i + 1} 条规则没有行号（line），无法定点改写。` +
          `行号来自 config_get 的 rules[].line，在前端丢掉它就等于放弃保留注释的编辑。`,
      );
    }
    if (typeof at.raw !== 'string' || typeof now?.raw !== 'string') {
      throw new Error(
        `第 ${i + 1} 条规则没有原文（raw），无法给出 expect。` +
          `expect 必须是 config_get 快照里那一行的原值，不能由前端state 拼凑。`,
      );
    }
    if (at.raw === now.raw) continue; // 这一行的值没变，不必写

    ops.push({ op: 'replace-rule', line: at.line, expect: at.raw, value: now.raw });
  }
  return ops;
}

