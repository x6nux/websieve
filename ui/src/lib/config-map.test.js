import { describe, it, expect } from 'vitest';
import {
  toCmdError,
  whereOf,
  parseRuleLine,
  outboundStateOf,
  mergeOutbound,
  mergeConfigProxies,
  commentStart,
  setScalar,
  setGroupSelected,
  setNestedScalar,
  defaultInsertAnchor,
  ruleTypeToFormType,
  decisionOf,
  probeResultOf,
} from './config-map.js';

describe('toCmdError —— 任何异常都要能被显示出来', () => {
  it('CmdError 原样带过（kind 一路到视图）', () => {
    const e = toCmdError({ kind: 'not-ready', message: 'rule_test 尚未接入' });
    expect(e).toEqual({ kind: 'not-ready', message: 'rule_test 尚未接入' });
  });

  it('语法错带上行列号 —— §12 要求 UI 指出出错的行', () => {
    const e = toCmdError({ kind: 'config-syntax', message: '坏了', line: 13, column: 5 });
    expect(e.line).toBe(13);
    expect(e.column).toBe(5);
  });

  it('普通 Error 翻成 other，不丢消息', () => {
    const e = toCmdError(new Error('window.__TAURI__ is undefined'));
    expect(e.kind).toBe('other');
    expect(e.message).toMatch(/__TAURI__/);
  });

  it('字符串异常也有 kind', () => {
    expect(toCmdError('炸了').kind).toBe('other');
    expect(toCmdError('炸了').message).toBe('炸了');
  });

  it('message 缺失时不产出 "undefined" 这个词', () => {
    // 显示成「失败：undefined」的界面对排查毫无帮助，且看起来像是程序 bug
    expect(toCmdError({ kind: 'io' }).message).toBe('');
  });

  it('null / undefined 不抛错', () => {
    expect(toCmdError(null).kind).toBe('other');
    expect(toCmdError(undefined).kind).toBe('other');
  });
});

describe('whereOf —— 没有位置就一个字都不说', () => {
  it('语法错给出行列', () => {
    expect(whereOf({ kind: 'config-syntax', line: 7, column: 2 })).toBe('（第 7 行第 2 列）');
  });

  it('非语法错不加后缀', () => {
    expect(whereOf({ kind: 'io', message: 'x' })).toBe('');
  });

  it('语法错但没行号时不写「第 undefined 行」', () => {
    expect(whereOf({ kind: 'config-syntax', message: 'x' })).toBe('');
    // line=0 是「上游没能给出位置」的约定（见 CmdError::ConfigSyntax 注释）
    expect(whereOf({ kind: 'config-syntax', line: 0, column: 0 })).toBe('');
  });

  it('传 null 不抛错', () => {
    expect(whereOf(null)).toBe('');
  });
});

describe('parseRuleLine —— 认不出的东西不猜', () => {
  const names = new Set(['日本节点', '新加坡']);
  const at = (value, line = 1) => parseRuleLine({ value, line }, 0, names);

  it('三段规则拆成 type / value / target', () => {
    const r = at('DOMAIN-SUFFIX,googleapis.com,新加坡');
    expect(r.type).toBe('suffix');
    expect(r.value).toBe('googleapis.com');
    expect(r.target).toBe('新加坡');
  });

  it('MATCH 是两段，匹配值显示为 *', () => {
    const r = at('MATCH,日本节点');
    expect(r.type).toBe('match');
    expect(r.value).toBe('*');
    expect(r.target).toBe('日本节点');
  });

  it('FINAL 是 MATCH 的别名（与 wsieve-route 的 Rule::parse 一致）', () => {
    const r = at('FINAL,日本节点');
    expect(r.value).toBe('*');
    expect(r.target).toBe('日本节点');
  });

  it('DOMAIN 削完不能变成空字符串', () => {
    expect(at('DOMAIN,x.com,DIRECT').type).toBe('domain');
  });

  it('raw 与 line 原样带下去 —— 定点改写的全部依据', () => {
    const r = parseRuleLine({ value: 'GEOSITE,cn,DIRECT', line: 42 }, 3, names);
    expect(r.raw).toBe('GEOSITE,cn,DIRECT');
    expect(r.line).toBe(42);
    expect(r.id).toBe(3);
  });

  it('段数不足时标 ? 并把整行原样交给用户，不编造 target', () => {
    // 猜一个 DIRECT 出来就是在界面上编造一条并不存在的判决
    const r = at('GEOSITE');
    expect(r.type).toBe('?');
    expect(r.value).toBe('GEOSITE');
    expect(r.target).toBe('');
  });

  it('目标为空串同样算解析不出来', () => {
    // wsieve-route 那边 EmptyTarget 是明确的错误，界面不该假装它有效
    const r = at('GEOSITE,cn,');
    expect(r.type).toBe('?');
    expect(r.target).toBe('');
  });

  it('空行不抛错', () => {
    const r = at('');
    expect(r.type).toBe('?');
    expect(r.target).toBe('');
  });

  it('引用了不存在的出站要标出来（§12）', () => {
    expect(at('DOMAIN,x.com,幽灵节点').unknownOutbound).toBe(true);
  });

  it('DIRECT / REJECT 不算「不存在的出站」', () => {
    expect(at('GEOSITE,cn,DIRECT').unknownOutbound).toBe(false);
    expect(at('GEOSITE,category-ads,REJECT').unknownOutbound).toBe(false);
  });

  it('解析失败的行不再额外报「出站不存在」—— 两条一起报会让用户找不到重点', () => {
    expect(at('GEOSITE').unknownOutbound).toBe(false);
  });

  it('类型大小写不敏感（与后端一致）', () => {
    expect(at('geosite,cn,DIRECT').type).toBe('geosite');
    expect(at('match,DIRECT').value).toBe('*');
  });

  it('段间空白被去掉', () => {
    const r = at('DOMAIN-SUFFIX , a.com , 新加坡');
    expect(r.value).toBe('a.com');
    expect(r.target).toBe('新加坡');
  });

  it('no-resolve 第四段不影响 target', () => {
    const r = at('IP-CIDR,10.0.0.0/8,DIRECT,no-resolve');
    expect(r.target).toBe('DIRECT');
    expect(r.type).toBe('ip-cidr');
  });

  it('hits 初始为 0 —— 靠 rule-hit 事件累加，首次打开热度全灰是预期的', () => {
    expect(at('GEOSITE,cn,DIRECT').hits).toBe(0);
  });

  it('enabled 恒为 true —— 配置格式里规则没有这个字段', () => {
    expect(at('GEOSITE,cn,DIRECT').enabled).toBe(true);
  });
});

describe('outboundStateOf —— 认不出的状态不猜成 live', () => {
  it('四个已知状态逐一映射', () => {
    expect(outboundStateOf('connecting')).toBe('starting');
    expect(outboundStateOf('live')).toBe('live');
    expect(outboundStateOf('failed')).toBe('failed');
    expect(outboundStateOf('disabled')).toBe('stopped');
  });

  it('认不出的一律 stopped —— 报一个比事实乐观的状态与 §6.4 冲突', () => {
    expect(outboundStateOf('reconnecting')).toBe('stopped');
    expect(outboundStateOf('')).toBe('stopped');
    expect(outboundStateOf(undefined)).toBe('stopped');
  });
});

describe('mergeOutbound —— 单条事件并进列表', () => {
  const list = [
    { id: 'JP', name: 'JP', state: 'stopped', latency: null, sessions: 2, enabled: true, host: true },
  ];

  it('按名字就地更新', () => {
    const out = mergeOutbound(list, { name: 'JP', state: 'live', latency_ms: 38 });
    expect(out).toHaveLength(1);
    expect(out[0].state).toBe('live');
    expect(out[0].latency).toBe(38);
  });

  it('保留事件里没有的字段 —— sessions / host 不该被事件冲掉', () => {
    const out = mergeOutbound(list, { name: 'JP', state: 'live', latency_ms: 38 });
    expect(out[0].sessions).toBe(2);
    expect(out[0].host).toBe(true);
  });

  it('认不出的名字追加一行 —— 丢掉它等于「出站起来了但列表里没有」', () => {
    const out = mergeOutbound(list, { name: 'SG', state: 'connecting', latency_ms: null });
    expect(out).toHaveLength(2);
    expect(out[1].name).toBe('SG');
    expect(out[1].state).toBe('starting');
  });

  it('新行的会话数是 0 而非编造的数字', () => {
    const out = mergeOutbound([], { name: 'SG', state: 'live', latency_ms: 10 });
    expect(out[0].sessions).toBe(0);
  });

  it('latency_ms 缺失时给 null 而非 0 —— 0 ms 会被读成「极快」', () => {
    const out = mergeOutbound([], { name: 'SG', state: 'connecting' });
    expect(out[0].latency).toBeNull();
  });

  it('无名字的事件被忽略且不损坏列表', () => {
    expect(mergeOutbound(list, {})).toBe(list);
    expect(mergeOutbound(list, null)).toBe(list);
  });

  it('不改写输入数组', () => {
    const copy = JSON.stringify(list);
    mergeOutbound(list, { name: 'JP', state: 'live', latency_ms: 1 });
    expect(JSON.stringify(list)).toBe(copy);
  });
});

describe('mergeConfigProxies —— 配置里写着不等于连上了', () => {
  const proxies = [{ name: '日本节点' }, { name: '新加坡' }];

  it('初始状态是 stopped 而非 live', () => {
    const out = mergeConfigProxies(proxies, []);
    expect(out.map((o) => o.state)).toEqual(['stopped', 'stopped']);
  });

  it('初始延迟是 null 而非 0', () => {
    expect(mergeConfigProxies(proxies, [])[0].latency).toBeNull();
  });

  it('已有的运行时状态被保留 —— 重读配置不该把出站打回未连接', () => {
    const existing = [{ name: '日本节点', state: 'live', latency: 38, sessions: 4, enabled: true }];
    const out = mergeConfigProxies(proxies, existing);
    expect(out[0].state).toBe('live');
    expect(out[0].latency).toBe(38);
    expect(out[0].sessions).toBe(4);
  });

  it('carrier-host 为空时宿主是第一个出站', () => {
    const out = mergeConfigProxies(proxies, [], '');
    expect(out[0].host).toBe(true);
    expect(out[1].host).toBe(false);
  });

  it('carrier-host 指名时宿主跟着它走', () => {
    const out = mergeConfigProxies(proxies, [], '新加坡');
    expect(out[0].host).toBe(false);
    expect(out[1].host).toBe(true);
  });

  it('配置里没有 proxies 时给空列表，不抛错', () => {
    expect(mergeConfigProxies([], [])).toEqual([]);
    expect(mergeConfigProxies(undefined, undefined)).toEqual([]);
  });

  it('配置里删掉的出站不会留在列表里', () => {
    const existing = [{ name: '已删除的', state: 'live', latency: 1, sessions: 1, enabled: true }];
    const out = mergeConfigProxies(proxies, existing);
    expect(out.map((o) => o.name)).toEqual(['日本节点', '新加坡']);
  });
});

describe('commentStart —— YAML 的注释起点', () => {
  it('行首的 # 是注释', () => expect(commentStart('# x')).toBe(0));
  it('空白后的 # 是注释', () => expect(commentStart('7890  # 端口')).toBe(6));
  it('紧贴值的 # 不是注释', () => expect(commentStart('shared#1')).toBe(-1));
  it('没有 # 时返回 -1', () => expect(commentStart('7890')).toBe(-1));
  it('制表符也算空白', () => expect(commentStart('7890\t#x')).toBe(5));
});

describe('setScalar —— 逐行改，其余字节原样不动', () => {
  const SRC = [
    '# websieve 配置',
    'mixed-port: 7890  # 混合入口',
    'allow-lan: false',
    '',
    'proxies:',
    '  - name: 日本节点',
    '    mixed-port: 1  # 同名的嵌套键，绝不能被改到',
    '',
    'rules:',
    '  # 广告一律拒绝',
    '  - GEOSITE,category-ads,REJECT',
    '  - MATCH,日本节点',
    '',
  ].join('\n');

  it('换掉顶层标量的值', () => {
    expect(setScalar(SRC, 'mixed-port', 1080)).toMatch(/^mixed-port: 1080/m);
  });

  it('行尾注释保留', () => {
    expect(setScalar(SRC, 'mixed-port', 1080)).toContain('mixed-port: 1080  # 混合入口');
  });

  it('**不碰**缩进的同名嵌套键 —— 改到那上面是静默损坏', () => {
    const out = setScalar(SRC, 'mixed-port', 1080);
    expect(out).toContain('    mixed-port: 1  # 同名的嵌套键，绝不能被改到');
  });

  it('规则区的注释一个字节都不动（§5.6 的核心承诺）', () => {
    const out = setScalar(SRC, 'allow-lan', true);
    expect(out).toContain('  # 广告一律拒绝');
    expect(out).toContain('  - GEOSITE,category-ads,REJECT');
    expect(out).toContain('  - MATCH,日本节点');
  });

  it('除目标行外每一行都逐字不变', () => {
    const before = SRC.split('\n');
    const after = setScalar(SRC, 'allow-lan', true).split('\n');
    expect(after).toHaveLength(before.length);
    for (let i = 0; i < before.length; i++) {
      if (before[i].startsWith('allow-lan:')) continue;
      expect(after[i]).toBe(before[i]);
    }
  });

  it('布尔值不被加引号 —— 带引号的 "true" 在强类型解析下不是布尔', () => {
    expect(setScalar(SRC, 'allow-lan', true)).toMatch(/^allow-lan: true$/m);
  });

  it('键不存在时追加到末尾', () => {
    const out = setScalar(SRC, 'carrier', 'isolated');
    expect(out).toMatch(/^carrier: isolated$/m);
    // 原有内容一行不少
    expect(out).toContain('mixed-port: 7890  # 混合入口');
  });

  it('文件不以换行结尾时追加不会拼到最后一行屁股上', () => {
    const out = setScalar('mode: rule', 'carrier', 'shared');
    expect(out.split('\n')).toContain('mode: rule');
    expect(out).toMatch(/^carrier: shared$/m);
  });

  it('空文件也能追加', () => {
    expect(setScalar('', 'carrier', 'shared')).toBe('carrier: shared\n');
  });

  it('只改第一处匹配 —— 顶层重复键是坏配置，不该被悄悄改成两份不同的值', () => {
    const dup = 'mode: rule\nmode: global\n';
    const out = setScalar(dup, 'mode', 'direct');
    expect(out).toBe('mode: direct\nmode: global\n');
  });

  it('前缀相同的键不被误伤', () => {
    // `mixed-port` 与 `mixed-port-extra` 不是同一个键
    const src = 'mixed-port-extra: 1\nmixed-port: 7890\n';
    const out = setScalar(src, 'mixed-port', 1080);
    expect(out).toBe('mixed-port-extra: 1\nmixed-port: 1080\n');
  });

  it('CRLF 文件不被腰斩，\\r 既不进值里也不被吞掉', () => {
    // split('\n') 后每行尾留着 \r。它进了值就写成 `1080\r`（YAML 读回来是
    // 带回车的字符串，端口解析当场失败）；被吞掉则这一行的换行风格与文件
    // 其余部分不一致，用户的 diff 上会多出一整行噪音。
    const out = setScalar('mixed-port: 7890\r\nmode: rule\r\n', 'mixed-port', 1080);
    expect(out).toBe('mixed-port: 1080\r\nmode: rule\r\n');
  });

  it('CRLF 下的行尾注释同样完整保留', () => {
    const out = setScalar('mixed-port: 7890  # 入口\r\n', 'mixed-port', 1080);
    expect(out).toBe('mixed-port: 1080  # 入口\r\n');
  });
});

describe('setGroupSelected', () => {
  const src = [
    'proxy-groups:',
    '  - name: 节点选择',
    '    kind: select',
    '    proxies: [日本节点, 香港节点]',
    '    selected: 日本节点',
    '  - name: 自动选优',
    '    kind: auto',
    '    proxies: [日本节点, 香港节点]',
    'rules: []',
    '',
  ].join('\n');

  it('只改目标组的 selected，不动别的组', () => {
    const out = setGroupSelected(src, '节点选择', '香港节点');
    expect(out).toContain('    selected: 香港节点');
    expect(out).toContain('  - name: 自动选优');
    const autoBlockLines = out
      .split('\n')
      .slice(out.split('\n').indexOf('  - name: 自动选优'));
    expect(autoBlockLines.some((l) => l.includes('selected:'))).toBe(false);
  });

  it('保留行尾注释', () => {
    const withComment = src.replace(
      '    selected: 日本节点',
      '    selected: 日本节点  # 默认走这个',
    );
    const out = setGroupSelected(withComment, '节点选择', '香港节点');
    expect(out).toContain('    selected: 香港节点  # 默认走这个');
  });

  it('其余组与其余内容一字节不变', () => {
    const out = setGroupSelected(src, '节点选择', '香港节点');
    expect(out).toContain('  - name: 自动选优\n    kind: auto\n    proxies: [日本节点, 香港节点]\nrules: []');
  });

  it('找不到匹配的组名时如实报错', () => {
    expect(() => setGroupSelected(src, '不存在的组', '日本节点')).toThrow(/不存在的组/);
  });

  it('组存在但没有 selected 行（比如 auto 类型）时如实报错，不静默无操作', () => {
    expect(() => setGroupSelected(src, '自动选优', '日本节点')).toThrow(/selected/);
  });

  it('没有 proxy-groups 键时如实报错', () => {
    expect(() => setGroupSelected('rules: []\n', '节点选择', '日本节点')).toThrow(/proxy-groups/);
  });

  it('proxies 写成块式列表也能正确找到 selected', () => {
    // proxies 的嵌套 `- ` 行缩进比组项本身（2）更深（6），不该被当成
    // 下一个同级组的边界，否则扫描会在碰到它时提前截断，永远走不到 selected。
    const blockStyle = [
      'proxy-groups:',
      '  - name: 节点选择',
      '    kind: select',
      '    proxies:',
      '      - 日本节点',
      '      - 香港节点',
      '    selected: 日本节点',
      '  - name: 自动选优',
      '    kind: auto',
      '    proxies:',
      '      - 日本节点',
      '      - 香港节点',
      'rules: []',
      '',
    ].join('\n');
    const out = setGroupSelected(blockStyle, '节点选择', '香港节点');
    expect(out).toContain('    selected: 香港节点');
    expect(out).toContain('      - 日本节点');
    expect(out).toContain('  - name: 自动选优');
  });

  it('CRLF 文件下也能定位到目标组并保留 \\r\\n，其余行不动', () => {
    const crlf = src.replace(/\n/g, '\r\n');
    const out = setGroupSelected(crlf, '节点选择', '香港节点');
    expect(out).toBe(crlf.replace('    selected: 日本节点\r\n', '    selected: 香港节点\r\n'));
    expect(out).toContain('    selected: 香港节点\r\n');
    expect(out).toContain('  - name: 自动选优\r\n');
  });
});

describe('setNestedScalar —— tun.enable 这类嵌套标量', () => {
  const src = [
    '# websieve 配置',
    'mixed-port: 7890',
    'system-proxy: false',
    'tun:',
    '  enable: false',
    '  stack: system  # 默认栈',
    '  auto-route: true',
    'dns:',
    '  enable: true',
    'rules: []',
    '',
  ].join('\n');

  it('换掉嵌套字段的值', () => {
    const out = setNestedScalar(src, 'tun', 'enable', true);
    expect(out).toMatch(/^ {2}enable: true$/m);
  });

  it('保留行尾注释', () => {
    const out = setNestedScalar(src, 'tun', 'stack', 'gvisor');
    expect(out).toContain('  stack: gvisor  # 默认栈');
  });

  it('同一 parent 块下的其他兄弟字段不动', () => {
    const out = setNestedScalar(src, 'tun', 'enable', true);
    expect(out).toContain('  stack: system  # 默认栈');
    expect(out).toContain('  auto-route: true');
  });

  it('其余顶层键（包括另一个同名子键 dns.enable）一字节不变', () => {
    const out = setNestedScalar(src, 'tun', 'enable', true);
    expect(out).toContain('mixed-port: 7890');
    expect(out).toContain('system-proxy: false');
    expect(out).toContain('dns:\n  enable: true');
    expect(out).toContain('rules: []');
  });

  it('parentKey 不存在时追加一个新块 —— 新配置默认没有 tun: 键，报错会让开关彻底不能用', () => {
    const out = setNestedScalar('mixed-port: 25500\nproxies: []\n', 'tun', 'enable', true);
    expect(out).toContain('mixed-port: 25500\n');
    expect(out).toContain('proxies: []\n');
    expect(out).toMatch(/tun:\n {2}enable: true\n$/);
  });

  it('parentKey 不存在时，追加前先确保有换行分隔 —— 与 setScalar 同一条纪律', () => {
    const out = setNestedScalar('mode: rule', 'tun', 'enable', true);
    expect(out).toBe('mode: rule\ntun:\n  enable: true\n');
  });

  it('parentKey 存在但块内没有 childKey 时插入新行，保留块内已有字段', () => {
    const withoutEnable = ['tun:', '  stack: gvisor', 'rules: []', ''].join('\n');
    const out = setNestedScalar(withoutEnable, 'tun', 'enable', true);
    expect(out).toContain('  stack: gvisor');
    expect(out).toMatch(/tun:\n(?:.*\n)*? {2}enable: true\n/);
    expect(out).toContain('rules: []');
  });

  it('parentKey 存在但块是空的（后面紧接下一个顶层键）时也能插入 childKey', () => {
    const empty = ['tun:', 'rules: []', ''].join('\n');
    const out = setNestedScalar(empty, 'tun', 'enable', true);
    expect(out).toContain('tun:\n  enable: true\nrules: []');
  });

  it('CRLF 文件下也能定位并保留 \\r\\n，其余行不动', () => {
    const crlf = src.replace(/\n/g, '\r\n');
    const out = setNestedScalar(crlf, 'tun', 'enable', true);
    expect(out).toBe(crlf.replace('  enable: false\r\n', '  enable: true\r\n'));
  });
});

describe('decisionOf / probeResultOf —— 探针结果的翻译', () => {
  it('DIRECT / REJECT 走固定状态色，其余是出站', () => {
    expect(decisionOf('DIRECT')).toBe('Direct');
    expect(decisionOf('REJECT')).toBe('Reject');
    expect(decisionOf('日本节点')).toBe('Outbound');
  });

  it('index 从 0-based 翻成界面说的「第 N 条」', () => {
    const r = probeResultOf({ index: 2, decision: '日本节点', tried: 2, resolved: null });
    expect(r.index).toBe(3);
  });

  it('outbound 保留原名 —— 色码要按它取', () => {
    const r = probeResultOf({ index: 0, decision: '日本节点', tried: 0, resolved: null });
    expect(r.outbound).toBe('日本节点');
  });

  it('第一轮判完且没要求解析 → 提示需解析', () => {
    const r = probeResultOf({ index: 4, decision: 'DIRECT', tried: 4, resolved: null }, false);
    expect(r.needResolve).toBe(true);
  });

  it('已经跑过两轮就不再提示 —— 否则用户陷入无限重测', () => {
    const r = probeResultOf({ index: 4, decision: 'DIRECT', tried: 4, resolved: null }, true);
    expect(r.needResolve).toBe(false);
  });

  it('真的解析过（resolved 是数组）就不提示', () => {
    const r = probeResultOf({ index: 1, decision: 'DIRECT', tried: 1, resolved: ['1.2.3.4'] }, false);
    expect(r.needResolve).toBe(false);
  });

  it('没有结果时返回 null 而非一个空壳判决', () => {
    expect(probeResultOf(null)).toBeNull();
    expect(probeResultOf(undefined)).toBeNull();
  });
});

describe('defaultInsertAnchor', () => {
  it('规则列表为空时，锚点是 rules 键本身', () => {
    const a = defaultInsertAnchor([], 9, 'rules: []');
    expect(a).toEqual({ anchor: 9, anchorExpect: 'rules: []' });
  });

  it('末条不是 MATCH 时，新规则接在最后一条规则之后', () => {
    const rules = [
      { line: 10, raw: 'DOMAIN,a.com,proxyA' },
      { line: 11, raw: 'DOMAIN,b.com,proxyB', type: 'domain' },
    ];
    const a = defaultInsertAnchor(rules, 9, 'rules:');
    expect(a).toEqual({ anchor: 11, anchorExpect: 'DOMAIN,b.com,proxyB' });
  });

  it('末条是 MATCH 且前面还有别的规则时，新规则接在 MATCH 前一条之后', () => {
    const rules = [
      { line: 10, raw: 'DOMAIN,a.com,proxyA', type: 'domain' },
      { line: 11, raw: 'MATCH,proxyB', type: 'match' },
    ];
    const a = defaultInsertAnchor(rules, 9, 'rules:');
    expect(a).toEqual({ anchor: 10, anchorExpect: 'DOMAIN,a.com,proxyA' });
  });

  it('只有一条 MATCH 兜底时，新规则的锚点回退到 rules 键（成为新的第一条）', () => {
    const rules = [{ line: 10, raw: 'MATCH,proxyB', type: 'match' }];
    const a = defaultInsertAnchor(rules, 9, 'rules:');
    expect(a).toEqual({ anchor: 9, anchorExpect: 'rules:' });
  });

  it('末条是 FINAL（MATCH 的别名）且前面还有别的规则时，新规则同样接在它前一条之后 —— 不是插到 FINAL 之后', () => {
    // FINAL 是 MATCH 的别名（Rule::parse 同构），parseRuleLine 给它的展示
    // 类型是 'final' 而非 'match'。插到 FINAL 之后是 RuleAfterMatch 明确
    // 拒绝的非法状态，这条锁定该情形不会被漏判。
    const rules = [
      { line: 10, raw: 'DOMAIN,a.com,proxyA', type: 'domain' },
      { line: 11, raw: 'FINAL,DIRECT', type: 'final' },
    ];
    const a = defaultInsertAnchor(rules, 9, 'rules:');
    expect(a).toEqual({ anchor: 10, anchorExpect: 'DOMAIN,a.com,proxyA' });
  });
});

describe('ruleTypeToFormType', () => {
  it('把 parseRuleLine 产出的展示用短写映射回 RuleForm 认的规则类型', () => {
    expect(ruleTypeToFormType('domain')).toBe('DOMAIN');
    expect(ruleTypeToFormType('suffix')).toBe('DOMAIN-SUFFIX');
    expect(ruleTypeToFormType('keyword')).toBe('DOMAIN-KEYWORD');
    expect(ruleTypeToFormType('ip-cidr')).toBe('IP-CIDR');
    expect(ruleTypeToFormType('geosite')).toBe('GEOSITE');
    expect(ruleTypeToFormType('geoip')).toBe('GEOIP');
    expect(ruleTypeToFormType('match')).toBe('MATCH');
    expect(ruleTypeToFormType('final')).toBe('MATCH');
  });

  it('认不出的短写（解析失败的 "?"）保守地落到 DOMAIN，而不是抛异常打断编辑', () => {
    expect(ruleTypeToFormType('?')).toBe('DOMAIN');
  });
});
