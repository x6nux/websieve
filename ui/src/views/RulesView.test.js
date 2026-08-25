import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import RulesView from './RulesView.svelte';

const rules = [
  { id: 1, type: 'geosite', value: 'category-ads', target: 'REJECT', hits: 18204, enabled: true },
  { id: 2, type: 'suffix', value: 'googleapis.com', target: '新加坡', hits: 3891, enabled: true },
  { id: 3, type: 'keyword', value: 'google', target: '日本节点', hits: 9417, enabled: true },
  { id: 4, type: 'geosite', value: 'cn', target: 'DIRECT', hits: 42663, enabled: true },
  { id: 5, type: 'final', value: '*', target: '日本节点', hits: 88120, enabled: true },
];
const colorOf = () => '#5b8ff9';
const base = () => ({
  rules,
  colorOf,
  probe: null,
  ontest: vi.fn(),
  onreorder: vi.fn(),
  ontoggle: vi.fn(),
});

describe('规则视图', () => {
  it('是一个带表头的列表，能被屏幕阅读器当表读', () => {
    render(RulesView, base());
    expect(screen.getByRole('table')).toBeInTheDocument();
    expect(screen.getAllByRole('columnheader').length).toBeGreaterThanOrEqual(4);
  });

  it('规则类型是低对比灰文本，不是彩色 pill', () => {
    // 反 AI 塑料感 + spec §11.4 明确拒绝：颜色全部让给出站
    const { container } = render(RulesView, base());
    const type = container.querySelector('.type');
    expect(type).toBeTruthy();
    const cls = type.getAttribute('class') ?? '';
    expect(cls).not.toMatch(/pill|badge|tag-/);
  });

  it('类型列不带任何内联颜色 —— 彩色会把列表变成彩虹糖', () => {
    const { container } = render(RulesView, base());
    for (const el of container.querySelectorAll('.type')) {
      expect(el.getAttribute('style') ?? '').not.toMatch(/color|background/);
    }
  });

  it('命中数越高背景越亮（signature ①）', () => {
    const { container } = render(RulesView, base());
    const rows = [...container.querySelectorAll('tbody tr')];
    const alpha = (el) => {
      const m = (el.getAttribute('style') ?? '').match(/rgba\(255,\s*255,\s*255,\s*([\d.]+)\)/);
      return m ? parseFloat(m[1]) : 0;
    };
    // final（88,120）应比 suffix（3,891）亮
    expect(alpha(rows[4])).toBeGreaterThan(alpha(rows[1]));
  });

  it('零命中的规则背景是透明的 —— 死规则要一眼可辨', () => {
    const dead = [
      ...rules,
      { id: 6, type: 'domain', value: 'dead.com', target: 'DIRECT', hits: 0, enabled: true },
    ];
    const { container } = render(RulesView, { ...base(), rules: dead });
    const last = [...container.querySelectorAll('tbody tr')].at(-1);
    const style = last.getAttribute('style') ?? '';
    expect(style).toMatch(/transparent|rgba\(255,\s*255,\s*255,\s*0\)/);
  });

  it('探针命中时该行被标出，其余降噪', () => {
    render(RulesView, {
      ...base(),
      probe: { index: 3, decision: 'Outbound', outbound: '日本节点', tried: 2, needResolve: false },
    });
    const rows = screen.getAllByRole('row').slice(1);
    expect(rows[2].className).toMatch(/hit/);
    expect(rows[0].className).toMatch(/dim/);
  });

  it('探针激活时一行都不删 —— 它是探针不是过滤器', () => {
    // signature ② 的全部价值在于「命中的是第 3 条，而前 2 条已试未命中」。
    // 把不匹配的行删掉，这个上下文就没了，它也就退化成一个搜索框。
    render(RulesView, {
      ...base(),
      probe: { index: 3, decision: 'Outbound', outbound: '日本节点', tried: 2, needResolve: false },
    });
    expect(screen.getAllByRole('row').slice(1)).toHaveLength(rules.length);
  });

  it('命中行用 aria-current 标注 —— 不只靠颜色', () => {
    render(RulesView, {
      ...base(),
      probe: { index: 3, decision: 'Outbound', outbound: 'JP', tried: 2, needResolve: false },
    });
    const rows = screen.getAllByRole('row').slice(1);
    expect(rows[2]).toHaveAttribute('aria-current', 'true');
  });

  it('每行有启用开关，且开关有可访问名字', () => {
    render(RulesView, base());
    const sw = screen.getAllByRole('switch');
    expect(sw).toHaveLength(rules.length);
    expect(sw[0]).toHaveAccessibleName(/启用|category-ads/);
  });

  it('点开关触发 ontoggle', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RulesView, p);
    await u.click(screen.getAllByRole('switch')[0]);
    expect(p.ontoggle).toHaveBeenCalledWith(1, false);
  });

  it('Alt+↓ 把规则下移（拖拽的键盘等价物）', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RulesView, p);
    const handle = screen.getAllByRole('button', { name: /移动|拖拽/ })[0];
    handle.focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');
    expect(p.onreorder).toHaveBeenCalledWith(0, 1);
  });

  it('Alt+↑ 把规则上移', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RulesView, p);
    screen.getAllByRole('button', { name: /移动/ })[2].focus();
    await u.keyboard('{Alt>}{ArrowUp}{/Alt}');
    expect(p.onreorder).toHaveBeenCalledWith(2, 1);
  });

  it('不按 Alt 的方向键不排序 —— 否则光标浏览会误改配置', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RulesView, p);
    screen.getAllByRole('button', { name: /移动/ })[1].focus();
    await u.keyboard('{ArrowDown}');
    await u.keyboard('{ArrowUp}');
    expect(p.onreorder).not.toHaveBeenCalled();
  });

  it('首项再上移不触发排序，也不静默', async () => {
    const u = userEvent.setup();
    const p = base();
    const { container } = render(RulesView, p);
    screen.getAllByRole('button', { name: /移动/ })[0].focus();
    await u.keyboard('{Alt>}{ArrowUp}{/Alt}');
    expect(p.onreorder).not.toHaveBeenCalled();
    // 到头了要说一声，否则用户以为按键没生效会继续猛按
    expect(container.querySelector('[aria-live="polite"][role="status"]').textContent).toMatch(
      /已在最前/,
    );
  });

  it('末项再下移不触发排序', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RulesView, p);
    screen.getAllByRole('button', { name: /移动/ }).at(-1).focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');
    expect(p.onreorder).not.toHaveBeenCalled();
  });

  it('拖拽把手对键盘可达', () => {
    render(RulesView, base());
    const handles = screen.getAllByRole('button', { name: /移动|拖拽/ });
    expect(handles).toHaveLength(rules.length);
    for (const h of handles) expect(h.tabIndex).toBeGreaterThanOrEqual(0);
  });

  it('把手的可访问名字里带当前位置与操作方法', () => {
    // 键盘用户看不到「第几行」这个视觉信息，名字里不带就无从判断移到哪了
    render(RulesView, base());
    const h = screen.getAllByRole('button', { name: /移动/ })[0];
    const name = h.getAttribute('aria-label');
    expect(name).toMatch(/第 1 条/);
    expect(name).toMatch(/共 5 条/);
    expect(name).toMatch(/Alt/);
  });

  it('排序后播报新位置 —— 键盘路径没有拖拽的视觉反馈', async () => {
    const u = userEvent.setup();
    const { container } = render(RulesView, base());
    screen.getAllByRole('button', { name: /移动/ })[0].focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');
    const live = container.querySelector('[aria-live="polite"][role="status"]');
    expect(live).toBeTruthy();
    expect(live.textContent).toMatch(/第 2 条/);
    expect(live.textContent).toMatch(/共 5 条/);
  });

  it('播报区对屏幕阅读器可见、对眼睛不可见', () => {
    const { container } = render(RulesView, base());
    const live = container.querySelector('[aria-live="polite"][role="status"]');
    expect(live.className).toMatch(/sr-only/);
  });

  it('规则引用不存在的出站时标红（spec §12）', () => {
    const bad = [
      { id: 9, type: 'domain', value: 'x.com', target: 'GHOST', hits: 0, enabled: true, unknownOutbound: true },
    ];
    const { container } = render(RulesView, { ...base(), rules: bad });
    expect(container.querySelector('tbody tr').className).toMatch(/invalid/);
  });

  it('无规则时给空状态引导，不渲染空表', () => {
    render(RulesView, { ...base(), rules: [] });
    expect(screen.queryByRole('table')).toBeNull();
    // 标题与动作按钮都要在：只有标题等于说了「没有」却没说「怎么办」
    expect(screen.getByText(/还没有规则/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /添加/ })).toBeInTheDocument();
  });

  it('空规则集不编造任何示例规则', () => {
    const { container } = render(RulesView, { ...base(), rules: [] });
    expect(container.querySelectorAll('tbody tr')).toHaveLength(0);
    expect(container.textContent).not.toMatch(/DOMAIN-SUFFIX|GEOSITE,/);
  });
});

describe('写回失败：绝不静默，绝不假装成功', () => {
  const err = (kind, message) => ({ ...base(), saveError: { kind, message } });

  it('config_save 被拒时明说「顺序未保存」', () => {
    render(RulesView, err('io', '写入 config.yaml 失败：磁盘已满'));
    expect(screen.getByText(/顺序未保存/)).toBeInTheDocument();
    expect(screen.getByText(/磁盘已满/)).toBeInTheDocument();
  });

  it('用 role=alert 而非 polite —— 顺序没落盘要立刻打断', () => {
    const { container } = render(RulesView, err('io', '写入失败'));
    expect(container.querySelector('[role="alert"]')).toBeTruthy();
  });

  it('说清文件里仍是旧顺序，且分流按文件走', () => {
    render(RulesView, err('io', '写入失败'));
    expect(screen.getByText(/文件里的顺序仍是改动前的那一份/)).toBeInTheDocument();
  });

  it('行号陈旧（config-invalid）时给出可照做的下一步', () => {
    // config_save 的并发校验：expect 对不上就拒绝，这时候让用户重试没用，
    // 得先刷新拿到新行号。
    render(
      RulesView,
      err('config-invalid', '第 13 行现在是 "MATCH,DIRECT"，而不是你看到的 "GEOSITE,cn,DIRECT"'),
    );
    expect(screen.getByText(/第 13 行现在是/)).toBeInTheDocument();
    expect(screen.getByText(/刷新后重试/)).toBeInTheDocument();
  });

  it('未就绪（not-ready）也照实显示，不吞', () => {
    render(RulesView, err('not-ready', 'set_mode 的落盘与生效 尚未接入（需要把 RuleSet 换成可热替换的）'));
    expect(screen.getByText(/尚未接入/)).toBeInTheDocument();
  });

  it('没有 saveError 时不显示任何告示', () => {
    const { container } = render(RulesView, base());
    expect(container.querySelector('[role="alert"]')).toBeNull();
  });

  it('保存失败不影响列表照常渲染与排序', async () => {
    const u = userEvent.setup();
    const p = err('io', '写入失败');
    render(RulesView, p);
    expect(screen.getAllByRole('row').slice(1)).toHaveLength(rules.length);
    screen.getAllByRole('button', { name: /移动/ })[0].focus();
    await u.keyboard('{Alt>}{ArrowDown}{/Alt}');
    expect(p.onreorder).toHaveBeenCalledWith(0, 1);
  });
});

describe('探针未就绪时规则视图仍可用', () => {
  it('rule_test 报 not-ready 不影响列表渲染', () => {
    render(RulesView, {
      ...base(),
      probeError: { kind: 'not-ready', message: 'rule_test 尚未接入（RuleSet 尚未进 managed state）' },
    });
    expect(screen.getByRole('table')).toBeInTheDocument();
    expect(screen.getAllByRole('row').slice(1)).toHaveLength(rules.length);
    expect(screen.getByText(/试算不可用/)).toBeInTheDocument();
  });

  it('探针未就绪时热度染色照常 —— 两个 signature 互不牵连', () => {
    const { container } = render(RulesView, {
      ...base(),
      probeError: { kind: 'not-ready', message: 'rule_test 尚未接入' },
    });
    const rows = [...container.querySelectorAll('tbody tr')];
    // jsdom 会把 style 属性里的 rgba 规范化成带空格的写法，正则得容忍
    expect(rows[4].getAttribute('style')).toMatch(/rgba\(255,\s*255,\s*255,\s*0\.05/);
  });
});
