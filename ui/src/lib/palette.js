/**
 * 出站色码分配（spec §11.4）。
 *
 * 出站色码是界面中**唯一允许出现的彩色**，它在规则行、流量表、桑基节点里
 * 指的永远是同一件事：去哪。因此分配必须全局一致且**与顺序无关** ——
 * 出站列表重排一下就全屏换色，会当场摧毁这条语义。
 *
 * DIRECT / REJECT 用固定状态色，不占出站色码：它们不是「一个出站」，
 * 而是两种处置方式。把它们塞进轮转里还会挤掉真出站的槽位，
 * 于是「加了一条 REJECT 规则，日本节点换了个颜色」。
 */

/**
 * 色码槽位数。**必须与 tokens.css 的 `--outbound-1..8` 严格相等。**
 *
 * 取模基数若比令牌数大，会生成 `var(--outbound-9)` 这种未定义的变量名 ——
 * CSS 变量未定义时 `background: var(--outbound-9)` **静默失效**，
 * 色块变透明，控制台一个字都不会说。导出成常量而非写死在下面的表达式里，
 * 就是为了让 palette.test.js 能把这条对齐关系直接断言掉。
 */
export const PALETTE_SLOTS = 8;

const BUILTIN = {
  DIRECT: 'var(--state-direct)',
  REJECT: 'var(--state-fail)',
};

/**
 * 与顺序无关的稳定哈希（FNV-1a 32 位）。
 *
 * 逐 UTF-16 码元而非逐字节：出站名是用户自由输入的中文，
 * 而 FNV 的乘法扩散足以让相邻码点散到不同槽位（palette.test.js 有断言）。
 */
function hash(s) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

/**
 * 返回 `(name) => CSS 色值`。
 *
 * 先按名字排序再分配槽位：在出站数 ≤ 8 时保证互不相同，
 * 且不受传入顺序影响。
 *
 * ponytail: 阶段 4 的 tokens.css 给了 8 个色码，第 9 个及以后会与前面重色。
 * 上限：两个出站同色时，颜色不再唯一标识去向，需靠文字区分（文字始终都在，
 * 见 a11y.test.js 的「色块非唯一载体」一组）。
 * 升级路径：改为按名字哈希到 HSL 色环（固定明度饱和度，只转色相）。
 * 现在不做 —— 自建服务器场景 3–5 个出站是常态，8 个已经很宽裕。
 */
export function makePalette(names = []) {
  const slot = new Map();
  const sorted = [...new Set(names)].filter((n) => !(n in BUILTIN)).sort();
  sorted.forEach((n, i) => slot.set(n, `var(--outbound-${(i % PALETTE_SLOTS) + 1})`));

  return (name) => {
    if (name in BUILTIN) return BUILTIN[name];
    const hit = slot.get(name);
    if (hit) return hit;
    // 未在列表里出现过的名字（如刚删掉的出站仍留在历史流量里、或规则指向
    // 一个不存在的出站）走哈希兜底 —— 返回 undefined 会让色块透明，
    // 而透明在这个界面里已经表示「从未命中」，两种含义会撞车。
    return `var(--outbound-${(hash(String(name)) % PALETTE_SLOTS) + 1})`;
  };
}
