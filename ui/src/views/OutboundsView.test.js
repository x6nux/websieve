import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import OutboundsView from './OutboundsView.svelte';

const outbounds = [
  { id: 'a', name: '日本节点', state: 'live', latency: 38, sessions: 4, enabled: true, host: true },
  { id: 'b', name: '新加坡', state: 'live', latency: 72, sessions: 4, enabled: true },
  { id: 'c', name: '德国备用', state: 'reconnecting', latency: null, sessions: 0, enabled: false },
];
const colorOf = () => '#5b8ff9';
const base = () => ({ outbounds, colorOf, ontoggle: vi.fn(), onprobe: vi.fn(), onadd: vi.fn() });

describe('出站列表', () => {
  it('是行列表而非卡片网格（spec §11.4 拒绝套路二）', () => {
    const { container } = render(OutboundsView, base());
    expect(container.querySelector('.grid, [class*="card"]')).toBeNull();
    expect(screen.getAllByRole('row').length).toBeGreaterThanOrEqual(3);
  });

  it('延迟与会话数用 tabular-nums 对齐成列', () => {
    const { container } = render(OutboundsView, base());
    const cell = container.querySelector('.num');
    expect(cell).toBeTruthy();
    expect(cell.className).toMatch(/\bnum\b/);
  });

  it('未测得延迟显示占位符而非 0 ms', () => {
    render(OutboundsView, base());
    expect(screen.queryByText('0 ms')).toBeNull();
  });

  it('状态不只靠颜色 —— 重连中必须有文字', () => {
    render(OutboundsView, base());
    expect(screen.getByText(/重连中/)).toBeInTheDocument();
  });

  it('宿主出站有标注', () => {
    render(OutboundsView, base());
    expect(screen.getByText(/宿主/)).toBeInTheDocument();
  });

  it('每行开关有可访问名字并能触发', async () => {
    const u = userEvent.setup();
    const p = base();
    render(OutboundsView, p);
    const sw = screen.getAllByRole('switch');
    expect(sw).toHaveLength(3);
    expect(sw[0]).toHaveAccessibleName(/日本节点/);
    await u.click(sw[0]);
    expect(p.ontoggle).toHaveBeenCalledWith('a', false);
  });

  it('有手动测延迟的按钮', async () => {
    const u = userEvent.setup();
    const p = base();
    render(OutboundsView, p);
    await u.click(screen.getAllByRole('button', { name: /测试延迟/ })[0]);
    expect(p.onprobe).toHaveBeenCalledWith('a');
  });

  it('每行状态用 aria-label 完整播报', () => {
    render(OutboundsView, base());
    const rows = screen.getAllByRole('row').slice(1);
    expect(rows[2].getAttribute('aria-label') ?? rows[2].textContent).toMatch(/重连/);
  });

  it('无出站时给「添加第一个服务器」引导（spec §11.3）', async () => {
    const u = userEvent.setup();
    const p = { ...base(), outbounds: [] };
    render(OutboundsView, p);
    const btn = screen.getByRole('button', { name: /添加第一个服务器/ });
    await u.click(btn);
    expect(p.onadd).toHaveBeenCalled();
  });

  it('失败状态显眼 —— §6.4 要求出站不可用时 UI 报警', () => {
    const bad = [{ id: 'x', name: 'X', state: 'failed', latency: null, sessions: 0, enabled: true }];
    const { container } = render(OutboundsView, { ...base(), outbounds: bad });
    expect(container.querySelector('.state-failed, [data-state="failed"]')).toBeTruthy();
  });
});

/**
 * 未就绪的两条命令。
 *
 * `outbound_enable` 与 `outbound_latency_probe` 在 src-tauri 里**当前确实**
 * 返回 `CmdError::NotReady`（出站管理器还在 run_stack 的局部作用域里，
 * 没暴露到命令面）。所以这不是假想的错误分支，是这个视图上线第一天就会
 * 走到的那一条。既不能吞掉（用户会以为开关生效了），也不能伪造成功。
 */
describe('出站列表 · 命令未就绪时', () => {
  it('启停失败时报警，并说清开关的视觉状态不代表实际', () => {
    render(OutboundsView, {
      ...base(),
      toggleError: { kind: 'not-ready', message: 'outbound_enable 尚未接入（出站的启停开关在阶段 2 的出站管理器里，尚未接到命令面）' },
    });
    const alerts = screen.getAllByRole('alert');
    expect(alerts.length).toBeGreaterThan(0);
    const text = alerts.map((a) => a.textContent).join(' ');
    expect(text).toMatch(/outbound_enable/);
    expect(text).toMatch(/未生效|没有生效|不代表/);
  });

  it('测速失败时报警，并保留「其余照常可读」的说明', () => {
    render(OutboundsView, {
      ...base(),
      probeError: { kind: 'not-ready', message: 'outbound_latency_probe 尚未接入（出站管理器尚未接到命令面）' },
    });
    const text = screen
      .getAllByRole('alert')
      .map((a) => a.textContent)
      .join(' ');
    expect(text).toMatch(/outbound_latency_probe/);
    // not-ready 要给出「什么还能用」，而不是只丢一句失败
    expect(text).toMatch(/仍然|照常|依旧/);
  });

  it('未就绪的报警不阻断列表本身 —— 出站行照常渲染', () => {
    render(OutboundsView, {
      ...base(),
      toggleError: { kind: 'not-ready', message: 'outbound_enable 尚未接入' },
      probeError: { kind: 'not-ready', message: 'outbound_latency_probe 尚未接入' },
    });
    expect(screen.getAllByRole('switch')).toHaveLength(3);
    expect(screen.getByText('日本节点')).toBeInTheDocument();
  });

  it('没有错误时不留空的报警壳子', () => {
    render(OutboundsView, base());
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('非 not-ready 的错误不会被套上「未就绪」的措辞', () => {
    render(OutboundsView, {
      ...base(),
      toggleError: { kind: 'config-invalid', message: '出站名不能为空' },
    });
    const text = screen
      .getAllByRole('alert')
      .map((a) => a.textContent)
      .join(' ');
    expect(text).toMatch(/出站名不能为空/);
    expect(text).not.toMatch(/尚未接入/);
  });
});

/**
 * 延迟指标的定义要如实呈现（spec §11.2）。
 * 稳态是「最近 N 次上行响应耗时的中位数」，不是一次 ping —— 用户不知道
 * 这个数字怎么来的，就会拿它跟 ping 的结果比，然后认为程序在撒谎。
 */
describe('出站列表 · 延迟的出处', () => {
  it('表头或说明里交代延迟的来源', () => {
    const { container } = render(OutboundsView, base());
    expect(container.textContent).toMatch(/中位数|握手/);
  });

  it('非 live 的出站不给测速按钮可点', () => {
    render(OutboundsView, base());
    const btns = screen.getAllByRole('button', { name: /测试延迟/ });
    // 第三个是重连中的德国备用
    expect(btns[2]).toBeDisabled();
  });
});
