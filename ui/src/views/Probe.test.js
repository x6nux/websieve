import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import Probe from './Probe.svelte';

const colorOf = () => '#5b8ff9';

describe('探针即搜索框（signature ②）', () => {
  it('输入框有可访问的名字，且不假装成搜索框', async () => {
    render(Probe, { result: null, colorOf, ontest: vi.fn() });
    const input = screen.getByRole('textbox');
    expect(input).toHaveAccessibleName(/试算|域名/);
  });

  it('输入后触发试算，默认不解析 DNS', async () => {
    const ontest = vi.fn();
    const u = userEvent.setup();
    render(Probe, { result: null, colorOf, ontest, debounce: 0 });
    await u.type(screen.getByRole('textbox'), 'a.com');
    await vi.waitFor(() => expect(ontest).toHaveBeenCalled());
    const [target, opts] = ontest.mock.calls.at(-1);
    expect(target).toBe('a.com');
    expect(opts.resolve).toBe(false);
  });

  it('显示命中第几条与判决', () => {
    render(Probe, {
      colorOf,
      ontest: vi.fn(),
      result: { index: 3, decision: 'Outbound', outbound: '日本节点', tried: 2, needResolve: false },
    });
    expect(screen.getByText(/第\s*3\s*条/)).toBeInTheDocument();
    expect(screen.getByText('日本节点')).toBeInTheDocument();
  });

  it('显示「前 N 条已试未命中」—— 这是排查的关键信息', () => {
    render(Probe, {
      colorOf,
      ontest: vi.fn(),
      result: { index: 3, decision: 'Outbound', outbound: 'JP', tried: 2, needResolve: false },
    });
    expect(screen.getByText(/前\s*2\s*条已试/)).toBeInTheDocument();
  });

  it('命中 IP 类规则时提示需解析，并给一键重测', async () => {
    const ontest = vi.fn();
    const u = userEvent.setup();
    render(Probe, {
      colorOf,
      ontest,
      result: { index: 5, decision: null, tried: 4, needResolve: true },
    });
    expect(screen.getByText(/需解析/)).toBeInTheDocument();
    const btn = screen.getByRole('button', { name: /解析后重测/ });
    await u.click(btn);
    expect(ontest.mock.calls.at(-1)[1].resolve).toBe(true);
  });

  it('判决结果用 aria-live 播报 —— 屏幕阅读器要能听到试算结论', () => {
    const { container } = render(Probe, {
      colorOf,
      ontest: vi.fn(),
      result: { index: 1, decision: 'Direct', tried: 0, needResolve: false },
    });
    expect(container.querySelector('[aria-live]')).toBeTruthy();
  });

  it('DIRECT / REJECT 判决也能正确显示', () => {
    render(Probe, {
      colorOf,
      ontest: vi.fn(),
      result: { index: 2, decision: 'Reject', tried: 1, needResolve: false },
    });
    expect(screen.getByText('REJECT')).toBeInTheDocument();
  });

  it('清空输入时不残留上一次的判决', async () => {
    const u = userEvent.setup();
    const ontest = vi.fn();
    render(Probe, { colorOf, ontest, result: null, debounce: 0 });
    const input = screen.getByRole('textbox');
    await u.type(input, 'a.com');
    await u.clear(input);
    await vi.waitFor(() => {
      const last = ontest.mock.calls.at(-1);
      expect(last[0]).toBe('');
    });
  });

  // ── 它凭什么不是搜索框：三条差别各守一条 ────────────────────

  it('标签是动词「试算」，不是搜索框的语汇', () => {
    // 放大镜、「搜索」、「过滤」、「清除」都是过滤器的语汇。
    // 摆上任何一个，用户就会按过滤器的预期去用它 ——
    // 然后发现列表一行没少，判断这个框「坏了」。
    const { container } = render(Probe, { colorOf, ontest: vi.fn(), result: null });
    const input = screen.getByRole('textbox');
    expect(screen.getByText('试算')).toBeInTheDocument();
    // 断言只针对**可操作元素的措辞**（标签、无障碍名、占位符），不扫全文 ——
    // 说明文案里的「列表不会被过滤」是刻意的否定句，把它一并禁掉就等于
    // 逼着实现去删掉那句最该说的话。
    for (const s of [
      input.getAttribute('aria-label'),
      input.getAttribute('placeholder'),
      screen.getByText('试算').textContent,
    ]) {
      expect(s).not.toMatch(/搜索|筛选|查找/);
    }
    expect(container.querySelector('svg')).toBeNull(); // 没有放大镜图标
  });

  it('输入框不是 type=search —— 那会带上浏览器的清除叉号', () => {
    render(Probe, { colorOf, ontest: vi.fn(), result: null });
    expect(screen.getByRole('textbox')).toHaveAttribute('type', 'text');
  });

  it('空闲时说明列表不会被过滤 —— 先把预期摆正', () => {
    render(Probe, { colorOf, ontest: vi.fn(), result: null });
    expect(screen.getByText(/不会被过滤/)).toBeInTheDocument();
  });

  // ── 命令未就绪：真实错误，既不吞也不伪造 ─────────────────

  it('rule_test 报 not-ready 时如实显示，不伪造判决', () => {
    render(Probe, {
      colorOf,
      ontest: vi.fn(),
      result: null,
      error: { kind: 'not-ready', message: 'rule_test 尚未接入（正在服役的 RuleSet 尚未进 managed state，见阶段 5）' },
    });
    expect(screen.getByText(/试算不可用/)).toBeInTheDocument();
    expect(screen.getByText(/RuleSet 尚未进 managed state/)).toBeInTheDocument();
    // 不能出现任何看起来像判决的东西
    expect(screen.queryByText(/命中第/)).toBeNull();
  });

  it('未就绪时说清「列表照常可用」—— 别让用户以为整个视图坏了', () => {
    render(Probe, {
      colorOf,
      ontest: vi.fn(),
      result: null,
      error: { kind: 'not-ready', message: 'rule_test 尚未接入' },
    });
    expect(screen.getByText(/列表照常可读可排序/)).toBeInTheDocument();
  });

  it('错误也走 aria-live —— 失败必须被播报，静默失败最坏', () => {
    const { container } = render(Probe, {
      colorOf,
      ontest: vi.fn(),
      result: null,
      error: { kind: 'io', message: '读取配置失败' },
    });
    const live = container.querySelector('[aria-live]');
    expect(live.textContent).toMatch(/读取配置失败/);
  });

  it('错误优先于陈旧结果显示 —— 拿上一次的判决盖住这一次的失败是撒谎', () => {
    render(Probe, {
      colorOf,
      ontest: vi.fn(),
      result: { index: 3, decision: 'Outbound', outbound: 'JP', tried: 2, needResolve: false },
      error: { kind: 'not-ready', message: 'rule_test 尚未接入' },
    });
    expect(screen.getByText(/试算不可用/)).toBeInTheDocument();
    expect(screen.queryByText(/命中第\s*3\s*条/)).toBeNull();
  });

  it('输入框被判决区描述 —— 屏幕阅读器聚焦时能听到当前结论', () => {
    render(Probe, {
      colorOf,
      ontest: vi.fn(),
      result: { index: 2, decision: 'Direct', tried: 1, needResolve: false },
    });
    const input = screen.getByRole('textbox');
    const id = input.getAttribute('aria-describedby');
    expect(id).toBeTruthy();
    expect(document.getElementById(id).textContent).toMatch(/命中第\s*2\s*条/);
  });
});
