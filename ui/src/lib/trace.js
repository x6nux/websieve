/**
 * hover 高亮的路径闭包（spec §11.6）。
 *
 * 「其余降至 16%」里的「其余」必须按**双向可达闭包**算，不能只看相邻：
 * hover 站点节点时，只比较 sourceId/targetId 相邻会漏掉「规则→出站」
 * 那一跳，路径视觉上断成半截（实测 12 条流带里 11 条 faded，其中一条是错的）。
 *
 * 返回 null 表示没有 hover —— 语义是「全部原样」，与「空集合」
 * （hover 到一个孤立节点，全部变暗）截然不同，调用方必须区分。
 */
export function tracePath(links, nodeId) {
  if (!nodeId) return null;

  const fwd = new Map();
  const bwd = new Map();
  for (const l of links) {
    if (!fwd.has(l.sourceId)) fwd.set(l.sourceId, []);
    if (!bwd.has(l.targetId)) bwd.set(l.targetId, []);
    fwd.get(l.sourceId).push(l);
    bwd.get(l.targetId).push(l);
  }

  const hot = new Set();
  // 用显式栈而非递归：真实数据下深度只有 2，但环形输入会让递归爆栈，
  // 而 hot 去重同时也是环的终止条件
  const walk = (start, adj, step) => {
    const stack = [start];
    while (stack.length) {
      const id = stack.pop();
      for (const l of adj.get(id) ?? []) {
        if (hot.has(l.key)) continue;
        hot.add(l.key);
        stack.push(step(l));
      }
    }
  };
  walk(nodeId, fwd, (l) => l.targetId);
  walk(nodeId, bwd, (l) => l.sourceId);
  return hot;
}
