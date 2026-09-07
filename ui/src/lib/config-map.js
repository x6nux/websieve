/**
 * 后端契约 → 视图形状的翻译层。
 *
 * 抽出来不是为了整洁，是为了**能被穷举单测**：这一层里的每一件事都是
 * 可判定的纯函数，而它们要是留在 `App.svelte` 里就只能靠渲染断言间接测 ——
 * 而渲染断言对「解析坏了的规则行会不会编造一个 DIRECT 出来」这类问题
 * 几乎无能为力。这与 lib/ 下其余模块是同一条纪律。
 *
 * 三条贯穿本模块的原则：
 *
 * ① **认不出的东西不猜。** 规则行解析不出来就标 `?` 并把整行原样交给用户看，
 *    出站状态词认不出就当 stopped（最保守的那个）。猜出来的值会在界面上
 *    表现为一条**并不存在的判决**或一个比事实乐观的状态。
 * ② **line 与 raw 一路带下去。** 它们是 `config_save` 定点改写的全部依据，
 *    在任何一环丢掉，保留注释的编辑就彻底做不到了。
 * ③ **改配置原文时逐行改，不重新序列化。** 没被碰过的行原样留下，
 *    规则区的注释因此一个字节都不会动。
 */

/**
 * 任何异常 → `{ kind, message }`（可能带 line/column）。
 *
 * Tauri 把命令的 `Err` 原样序列化过来，所以正常路径上拿到的就是 CmdError
 * （src-tauri/src/commands/mod.rs，`#[serde(tag = "kind", rename_all = "kebab-case")]`）。
 * 但 IPC 本身也会抛：窗口没有该命令的权限、参数序列化失败、
 * `window.__TAURI__` 压根不存在。那时拿到的是 Error 或字符串 ——
 * 一律翻成 `kind: 'other'`，绝不让一个 `message` 为 undefined 的对象漏进视图，
 * 在界面上显示成「失败：undefined」。
 */
export function toCmdError(e) {
  if (e && typeof e === 'object' && typeof e.kind === 'string') {
    const out = { kind: e.kind, message: String(e.message ?? '') };
    // 语法错带行列号 —— §12 要求 UI 能指出出错的行
    if (typeof e.line === 'number') out.line = e.line;
    if (typeof e.column === 'number') out.column = e.column;
    return out;
  }
  return { kind: 'other', message: String(e?.message ?? e) };
}

/** 语法错的位置后缀。没有位置就一个字都不说 —— 不写「第 undefined 行」。 */
export function whereOf(e) {
  return e?.kind === 'config-syntax' && e.line ? `（第 ${e.line} 行第 ${e.column} 列）` : '';
}

/** DIRECT / REJECT 不是出站，是两种处置方式。判断「这个目标是不是一个出站名」时要排除它们。 */
const BUILTIN_TARGETS = ['DIRECT', 'REJECT'];

/**
 * 一条规则原文 → 规则行。
 *
 * `raw` 与 `line` 原样带下去：前者是 `rule-hit` 事件的键（events.rs 的
 * `RuleHits` 用判决路径上的规则字符串做键），后者是定点改写的定位依据。
 *
 * type / value / target 只是**展示用的投影**。解析不出来时不猜：整行落到
 * 匹配值列里、类型标成 `?`，用户至少能看出是第几行写坏了。猜一个 DIRECT
 * 出来才是危险的 —— 那是在界面上编造一条并不存在的判决。
 *
 * @param {{value: string, line: number}} r0 config_get 的 rules[] 项
 * @param {number} i 下标，用作稳定的 key（排序时靠它找回焦点）
 * @param {Set<string>} names 配置里已声明的出站名
 */
export function parseRuleLine(r0, i, names = new Set()) {
  const raw = String(r0?.value ?? '');
  const parts = raw.split(',').map((x) => x.trim());
  const t = (parts[0] ?? '').toUpperCase();
  // MATCH / FINAL 是 2 段（TYPE,TARGET），其余至少 3 段。
  // 与 wsieve-route 的 `Rule::parse` 同构 —— 那边 FINAL 也是 MATCH 的别名。
  const isMatch = t === 'MATCH' || t === 'FINAL';
  const ok = isMatch ? parts.length >= 2 : parts.length >= 3;
  // 段数够但目标是空串（`GEOSITE,cn,`）同样算解析不出来：
  // 空出站名在 wsieve-route 那边是 EmptyTarget 错误，界面不该假装它有效
  const target = ok ? (isMatch ? parts[1] : parts[2]) : '';
  const good = ok && target !== '';
  return {
    id: i,
    raw,
    line: r0?.line,
    // 展示用短写：`domain-suffix` → `suffix`，省下的宽度给匹配值。
    // 单独的 `domain` 削完是空串，故有那个 `|| 'domain'` 兜底。
    type: good ? t.toLowerCase().replace(/^domain-?/, '') || 'domain' : '?',
    value: good ? (isMatch ? '*' : parts[1]) : raw,
    target: good ? target : '',
    hits: 0,
    /*
     * 规则没有「停用」这个状态。
     *
     * `Config.rules` 是 `Vec<Spanned<String>>` —— 一条规则要么在要么不在，
     * 格式里没有 enabled 字段。恒为 true 不是占位，它就是当前配置格式的事实。
     * 视图据此渲染开关，而 App 的 ontoggle 会如实报「这条路走不通」，
     * 绝不做一个切了以后什么都不会发生的假开关。
     */
    enabled: true,
    // §12：规则引用了不存在的出站要标红。空 target 不算「引用了不存在的出站」——
    // 那是解析失败，已经由 type='?' 表达了，两条一起报只会让用户找不到重点。
    unknownOutbound: good && !BUILTIN_TARGETS.includes(target) && !names.has(target),
  };
}

/**
 * 后端的出站状态词 → 视图的状态词。
 *
 * events.rs 的 `OutboundState.state` 是 "connecting" | "live" | "failed" |
 * "disabled"，而出站视图认的是 live / starting / reconnecting / failed / stopped。
 *
 * **认不出的值不猜成 live。** 猜成 live 就是在报一个比事实乐观的状态，
 * 与 §6.4「出站不可用时拒绝连接而非静默回退」同源。认不出一律当 stopped。
 */
const STATE_MAP = {
  connecting: 'starting',
  live: 'live',
  failed: 'failed',
  disabled: 'stopped',
};

export function outboundStateOf(s) {
  return STATE_MAP[s] ?? 'stopped';
}

/**
 * 把一条 `outbound-state` 事件并进出站列表。
 *
 * 事件是**单条**更新而不是全量列表，所以按名字就地改；认不出的名字追加一行 ——
 * 丢掉它等于「出站起来了但列表里没有」。
 *
 * 会话数后端还没上报，新行给 0：出站视图对 0 的呈现是占位符「—」，那是诚实的；
 * 编个 3 上去就是伪数据。已有行的 sessions / host 保留，不被事件冲掉。
 */
export function mergeOutbound(list, s) {
  if (!s?.name) return list;
  const next = {
    id: s.name,
    name: s.name,
    state: outboundStateOf(s.state),
    latency: typeof s.latency_ms === 'number' ? s.latency_ms : null,
    enabled: s.state !== 'disabled',
  };
  const i = list.findIndex((o) => o.name === s.name);
  if (i < 0) return [...list, { sessions: 0, host: false, ...next }];
  const out = [...list];
  out[i] = { ...out[i], ...next };
  return out;
}

/**
 * 配置里的 `proxies` → 出站行；已有的行保留其运行时状态。
 *
 * 初始状态是 `stopped` 而非 `live`：配置里写着不等于连上了。
 * 延迟给 null（视图显示占位符「—」）而不是 0 —— `0 ms` 会被读成「极快」。
 *
 * `carrier-host` 为空时宿主是第一个出站（spec §7：「空 = 取第一个启用的」）。
 */
export function mergeConfigProxies(proxies = [], existing = [], carrierHost = '') {
  const hostName = carrierHost || proxies[0]?.name;
  return proxies.map((p) => {
    const prev = existing.find((o) => o.name === p.name);
    return {
      id: p.name,
      name: p.name,
      state: prev?.state ?? 'stopped',
      latency: prev?.latency ?? null,
      sessions: prev?.sessions ?? 0,
      enabled: prev?.enabled ?? true,
      host: p.name === hostName,
    };
  });
}

/** 行尾注释的起点下标；没有则 -1。`#` 前必须是行首或空白才算注释（YAML 规则）。 */
export function commentStart(s) {
  for (let i = 0; i < s.length; i++) {
    if (s[i] === '#' && (i === 0 || /\s/.test(s[i - 1]))) return i;
  }
  return -1;
}

/**
 * 把**顶层**标量 `key` 的值换成 `value`，其余字节原样不动。
 *
 * ## 为什么需要它
 *
 * 设置里改的全是非规则区的标量（端口、allow-lan、carrier），而
 * `wsieve_config::edit` 只导出 `replace_rule_line` / `delete_rule_line` 两个
 * **规则行**改写器 —— 标量字段没有 `config_save` 的路可走，只能走
 * `config_save_raw`（整份覆盖）。整份覆盖若用序列化去生成，用户手写的
 * 注释与缩进会被全部归一化掉，而 §5.1 选 YAML 的唯一理由就是能写注释。
 *
 * 所以这里逐行改：认得出的那一行换值，其余行**连看都不看**地原样保留。
 * §5.6 那句「非规则区的注释会丢」因此只对**那一行的结构**成立，
 * 而不是整份文件。
 *
 * ## 三个刻意的选择
 *
 * - **只认行首无缩进的 `key:`**，因此不会误伤 `proxies:` 或 `dns:` 下面的
 *   同名嵌套键。改到嵌套键上是静默损坏：YAML 仍然合法，语义已经变了。
 * - **行尾注释连同它前面的对齐空白一起保留**，且注释起点按 YAML 规则判定
 *   （`#` 前必须是行首或空白）。否则 `carrier: shared#1` 里的 `#` 会被误当成
 *   注释起点，把值截断成 `shared`。对齐空白也不能吞：一列对齐的行尾注释被压成
 *   单空格，diff 上就是「我只改了端口，怎么整段都变了」——
 *   `wsieve_config::edit::replace_rule_line` 在规则行上守的正是同一条。
 * - **键不存在时追加到末尾**。Config 的全部字段都有 serde 默认值，
 *   「配置里没写这个键」是常态而非异常，报错反而拦住了正常操作。
 *
 * 结果由 Rust 侧复核：`config_save_raw` 写前会 `load_str` + `validate`，
 * 不合法会带着行列号被拒。
 */
export function setScalar(text, key, value) {
  const out = String(value);
  const head = `${key}:`;
  const lines = String(text).split('\n');
  for (let i = 0; i < lines.length; i++) {
    if (!lines[i].startsWith(head)) continue;
    let rest = lines[i].slice(head.length);
    // CRLF 文件按 '\n' 切开后每行尾留着 '\r'。它不是内容，先摘下来最后再贴回去 ——
    // 不摘的话它会被当成值的一部分写成 `1080\r`，或在无注释分支里被整个吞掉，
    // 让这一行的换行风格与文件其余部分不一致。
    const cr = rest.endsWith('\r') ? '\r' : '';
    if (cr) rest = rest.slice(0, -1);

    const c = commentStart(rest);
    if (c < 0) {
      lines[i] = `${head} ${out}${cr}`;
    } else {
      // 注释前的对齐空白照抄。一列对齐的行尾注释被压成单空格，diff 上就是
      // 「我只改了端口，怎么整段都变了」—— `edit.rs` 在规则行上守的正是同一条。
      const gap = /\s*$/.exec(rest.slice(0, c))[0];
      lines[i] = `${head} ${out}${gap}${rest.slice(c)}${cr}`;
    }
    return lines.join('\n');
  }
  // 末尾追加。文件若不以换行结尾，先补一个，否则会拼到最后一行屁股上，
  // 把那一行也一起改坏。
  const s = String(text);
  const sep = s === '' || s.endsWith('\n') ? '' : '\n';
  return `${s}${sep}${key}: ${out}\n`;
}

/**
 * 改写某个代理组的 `selected:` 字段——与 `setScalar` 的区别是 `selected:`
 * 这个键名可能在好几个组块里各出现一次，纯字符串匹配会串到别的组头上，
 * 必须先按 `groupName` 定位到具体是哪个组块，再只在那个范围内查找替换。
 *
 * 逐行文本操作，不重新序列化整份 YAML，其余组的内容与全部注释原样保留——
 * 与 `setScalar` 同一条纪律。
 */
export function setGroupSelected(text, groupName, member) {
  const lines = String(text).split('\n');
  const keyIdx = lines.findIndex((l) => l.replace(/\r$/, '') === 'proxy-groups:');
  if (keyIdx < 0) {
    throw new Error(`配置里没有 proxy-groups 键，找不到组 ${groupName}`);
  }

  let start = -1;
  // 目标项自己的缩进宽度——只有在这个宽度上的 `- ` 才是「下一个同级组」，
  // 缩进更深的 `- `（比如块式写法的 `proxies:` 列表元素）是目标块自己的
  // 内容，不是兄弟项的边界，与 `edit.rs` 的 `delete_proxy_block` 同一条纪律。
  let startIndent = 0;
  let end = lines.length;
  for (let i = keyIdx + 1; i < lines.length; i++) {
    const line = lines[i].replace(/\r$/, '');
    if (line.trim() === '') continue;
    const indent = line.length - line.trimStart().length;
    if (indent === 0) {
      end = i;
      break;
    }
    const trimmed = line.trimStart();
    if (trimmed.startsWith('- ')) {
      if (start >= 0) {
        if (indent === startIndent) {
          end = i;
          break;
        }
        // 缩进比目标项更深——是目标块内部嵌套列表（如块式 proxies）的元素，
        // 不是同级兄弟项，继续往下扫，一并纳入待查找范围。
      } else {
        const rest = trimmed.slice(2);
        if (rest.startsWith('name:')) {
          const name = rest.slice('name:'.length).trim().replace(/^"|"$/g, '');
          if (name === groupName) {
            start = i;
            startIndent = indent;
          }
        }
      }
    }
  }
  if (start < 0) {
    throw new Error(`找不到名为 ${groupName} 的代理组`);
  }

  const head = 'selected:';
  for (let i = start; i < end; i++) {
    const raw = lines[i];
    const cr = raw.endsWith('\r') ? '\r' : '';
    const line = cr ? raw.slice(0, -1) : raw;
    const trimmed = line.trimStart();
    if (!trimmed.startsWith(head)) continue;
    const indentStr = line.slice(0, line.length - trimmed.length);
    let rest = trimmed.slice(head.length);
    const c = commentStart(rest);
    if (c < 0) {
      lines[i] = `${indentStr}${head} ${member}${cr}`;
    } else {
      const gap = /\s*$/.exec(rest.slice(0, c))[0];
      lines[i] = `${indentStr}${head} ${member}${gap}${rest.slice(c)}${cr}`;
    }
    return lines.join('\n');
  }
  throw new Error(`代理组 ${groupName} 没有 selected 字段（不是 select 类型？）`);
}

/**
 * 改写**嵌套**标量 `parentKey.childKey` 的值——比如 `tun.enable`。
 *
 * ## 为什么不能用 `setScalar`
 *
 * `setScalar` 明确只认「行首无缩进的 `key:`」这一种形状。`tun` 这样的字段
 * 在 schema 里是个块（`tun:` 后面跟着缩进的 `enable:` / `stack:` / …），
 * 它的值不在 `tun:` 那一行上，文件里也**不存在**字面意义上的 `tun.enable:`
 * 这一行。所以 `setScalar(text, 'tun.enable', v)` 必然走到「键不存在→
 * 追加到文件末尾」那条分支，写出一个 schema 里根本没有的顶层字段
 * `tun.enable`——`Config` 上的 `#[serde(deny_unknown_fields)]` 会在下一次
 * `config_save_raw` 时把整份写入拒收，界面上就是一个「切换失败」。
 *
 * ## 定位方式
 *
 * 与 `setGroupSelected` 同一套纪律：先精确匹配顶层、零缩进的 `parentKey:`
 * 这一行（不是 `startsWith`，因为这里的值不在同一行上，不存在「后面还跟着
 * 别的字符」的情况，与 `setGroupSelected` 找 `proxy-groups:` 同理）；
 * 再把搜索范围限制在它的块内——下一个零缩进行（或 EOF）之前；
 * 只在这个范围内找 `childKey:`，缩进宽度照抄那一行实际写的缩进，
 * 不假设固定的 2 空格。
 *
 * 找不到 `parentKey:`，或者块内没有 `childKey:`，一律抛错，不静默 no-op——
 * 无声失败比报错更危险，用户会以为开关已经生效。
 */
export function setNestedScalar(text, parentKey, childKey, value) {
  const out = String(value);
  const lines = String(text).split('\n');
  const parentHead = `${parentKey}:`;
  const keyIdx = lines.findIndex((l) => l.replace(/\r$/, '') === parentHead);
  if (keyIdx < 0) {
    throw new Error(`配置里没有 ${parentKey} 键，找不到 ${parentKey}.${childKey}`);
  }

  // 块边界：下一个零缩进行（或 EOF）之前，都算 parentKey 自己的内容。
  let end = lines.length;
  for (let i = keyIdx + 1; i < lines.length; i++) {
    const line = lines[i].replace(/\r$/, '');
    if (line.trim() === '') continue;
    const indent = line.length - line.trimStart().length;
    if (indent === 0) {
      end = i;
      break;
    }
  }

  const head = `${childKey}:`;
  for (let i = keyIdx + 1; i < end; i++) {
    const raw = lines[i];
    const cr = raw.endsWith('\r') ? '\r' : '';
    const line = cr ? raw.slice(0, -1) : raw;
    const trimmed = line.trimStart();
    if (!trimmed.startsWith(head)) continue;
    const indentStr = line.slice(0, line.length - trimmed.length);
    let rest = trimmed.slice(head.length);
    const c = commentStart(rest);
    if (c < 0) {
      lines[i] = `${indentStr}${head} ${out}${cr}`;
    } else {
      const gap = /\s*$/.exec(rest.slice(0, c))[0];
      lines[i] = `${indentStr}${head} ${out}${gap}${rest.slice(c)}${cr}`;
    }
    return lines.join('\n');
  }
  throw new Error(`${parentKey} 块里没有 ${childKey} 字段`);
}

/**
 * 新增规则时默认的插入锚点——插在最后一条 MATCH 之前（若存在），否则接在
 * 末尾。理由：路由引擎的 `RuleSet::build` 要求 MATCH 必须是最后一条
 * （`RuleAfterMatch` 校验），插在它之后会被直接拒绝；`insert_rule_line`
 * 语义是「插在 anchor 行之后」，所以要选 MATCH **前一条**规则的行号当锚点，
 * 而不是 MATCH 自己的行号。
 *
 * 用户随后仍可以用既有的拖拽/Alt+↑↓ 把新规则挪到别的位置——这只是一个
 * 省得每次都要手动拖到底的默认值，不是强制位置。
 */
export function defaultInsertAnchor(rules, rulesKeyLine, rulesKeyText) {
  if (!rules.length) {
    return { anchor: rulesKeyLine, anchorExpect: rulesKeyText };
  }
  const lastIdx = rules.length - 1;
  const last = rules[lastIdx];
  if (last.type !== 'match') {
    return { anchor: last.line, anchorExpect: last.raw };
  }
  if (lastIdx === 0) {
    return { anchor: rulesKeyLine, anchorExpect: rulesKeyText };
  }
  const prev = rules[lastIdx - 1];
  return { anchor: prev.line, anchorExpect: prev.raw };
}

/**
 * `parseRuleLine` 产出的展示用短写（`domain` / `suffix` / `keyword` /
 * `ip-cidr` / `geosite` / `geoip` / `match` / `final` / `?`）→
 * `RuleForm` 的类型下拉认的大写规则类型。编辑一条已有规则时用来把
 * `rules[].type` 转回表单的初值。
 *
 * `?`（解析失败）没有对应的表单类型——保守落到 `DOMAIN`，让用户能打开
 * 表单把这条改成合法值，而不是抛异常拦住整个编辑入口。
 */
export function ruleTypeToFormType(t) {
  const MAP = {
    domain: 'DOMAIN',
    suffix: 'DOMAIN-SUFFIX',
    keyword: 'DOMAIN-KEYWORD',
    'ip-cidr': 'IP-CIDR',
    geosite: 'GEOSITE',
    geoip: 'GEOIP',
    match: 'MATCH',
    final: 'MATCH',
  };
  return MAP[t] ?? 'DOMAIN';
}

/**
 * `RuleTestResult.decision`（"DIRECT" | "REJECT" | 出站名）→ Probe 认的三个词。
 * 分开是因为 Probe 要给 DIRECT / REJECT 上固定的状态色，给出站上色码。
 */
export function decisionOf(d) {
  if (d === 'DIRECT') return 'Direct';
  if (d === 'REJECT') return 'Reject';
  return 'Outbound';
}

/**
 * `rule_test` 的返回 → Probe 的 result。
 *
 * 两处翻译，缺一不可：
 * - 后端的 `index` 是 0-based，界面说的「第 N 条」是 1-based
 * - `resolved === null` 表示第一轮就判完了。只有在**没要求解析**的前提下
 *   才提示「需解析」—— 已经跑过两轮还提示，用户会陷入无限重测
 */
export function probeResultOf(v, resolve = false) {
  if (!v) return null;
  return {
    index: v.index + 1,
    decision: decisionOf(v.decision),
    outbound: v.decision,
    tried: v.tried,
    needResolve: v.resolved === null && !resolve,
  };
}
