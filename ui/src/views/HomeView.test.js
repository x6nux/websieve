import { describe, it, expect, vi } from 'vitest';
import { render, screen, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import HomeView from './HomeView.svelte';

const colorOf = () => '#5b8ff9';

const base = () => ({
  groups: [],
  colorOf,
  allowLan: false,
  systemProxy: false,
  tunEnabled: false,
  preset: 'custom',
  spark: [10, 20, 5, 40, 15],
  downRate: 1024,
  upRate: 512,
  onselectmember: vi.fn(),
  onsystemproxychange: vi.fn(),
  ontunchange: vi.fn(),
  onpresetchange: vi.fn(),
});

describe('首页 —— 节点选择卡片', () => {
  it('零个 select 组时给出引导，不假装有一个组', () => {
    render(HomeView, base());
    expect(screen.getByText(/还没有配置代理组/)).toBeInTheDocument();
  });

  it('有一个 select 组时渲染成员下拉，当前选中项正确', () => {
    render(HomeView, {
      ...base(),
      groups: [{ name: '节点选择', kind: 'select', proxies: ['日本节点', '香港节点'], selected: '日本节点' }],
    });
    const select = screen.getByLabelText(/节点/);
    expect(select.value).toBe('日本节点');
  });

  it('切换成员触发 onselectmember，带上组名与新成员', async () => {
    const u = userEvent.setup();
    const p = {
      ...base(),
      groups: [{ name: '节点选择', kind: 'select', proxies: ['日本节点', '香港节点'], selected: '日本节点' }],
    };
    render(HomeView, p);
    await u.selectOptions(screen.getByLabelText(/节点/), '香港节点');
    expect(p.onselectmember).toHaveBeenCalledWith('节点选择', '香港节点');
  });

  it('多个 select 组时先选组、再选成员', async () => {
    const u = userEvent.setup();
    render(HomeView, {
      ...base(),
      groups: [
        { name: '节点选择', kind: 'select', proxies: ['日本节点'], selected: '日本节点' },
        { name: '备用组', kind: 'select', proxies: ['香港节点'], selected: '香港节点' },
      ],
    });
    const groupSelect = screen.getByLabelText(/代理组/);
    expect(groupSelect).toBeInTheDocument();
    await u.selectOptions(groupSelect, '备用组');
    expect(screen.getByLabelText(/节点/).value).toBe('香港节点');
  });

  it('auto / load-balance 类型的组不出现在节点选择卡片里', () => {
    render(HomeView, {
      ...base(),
      groups: [{ name: '自动选优', kind: 'auto', proxies: ['日本节点'] }],
    });
    expect(screen.getByText(/还没有配置代理组/)).toBeInTheDocument();
  });
});

describe('首页 —— 系统代理 / 虚拟网卡卡片', () => {
  it('两个开关反映当前状态', () => {
    render(HomeView, { ...base(), systemProxy: true, tunEnabled: false });
    expect(screen.getByRole('switch', { name: /系统代理/ })).toHaveAttribute('aria-checked', 'true');
    expect(screen.getByRole('switch', { name: /虚拟网卡|TUN/ })).toHaveAttribute('aria-checked', 'false');
  });

  it('切换系统代理触发 onsystemproxychange', async () => {
    const u = userEvent.setup();
    const p = base();
    render(HomeView, p);
    await u.click(screen.getByRole('switch', { name: /系统代理/ }));
    expect(p.onsystemproxychange).toHaveBeenCalledWith(true);
  });

  it('切换虚拟网卡触发 ontunchange', async () => {
    const u = userEvent.setup();
    const p = base();
    render(HomeView, p);
    await u.click(screen.getByRole('switch', { name: /虚拟网卡|TUN/ }));
    expect(p.ontunchange).toHaveBeenCalledWith(true);
  });
});

describe('首页 —— 分流模式卡片', () => {
  it('渲染与规则视图同一组预设选项，当前值正确', () => {
    render(HomeView, { ...base(), preset: 'china' });
    const group = screen.getByRole('radiogroup', { name: /分流预设/ });
    expect(within(group).getByRole('radio', { name: '中国大陆' })).toHaveAttribute('aria-checked', 'true');
  });

  it('切换预设触发 onpresetchange', async () => {
    const u = userEvent.setup();
    const p = base();
    render(HomeView, p);
    await u.click(screen.getByRole('radio', { name: '全局直连' }));
    expect(p.onpresetchange).toHaveBeenCalledWith('direct');
  });
});

describe('首页 —— 流量统计卡片', () => {
  it('显示上下行速率', () => {
    render(HomeView, { ...base(), downRate: 2048, upRate: 1024 });
    expect(screen.getByText(/2\.00 KB\/s|2 KB\/s/)).toBeInTheDocument();
  });

  it('sparkline 柱状条数与传入的采样点数一致', () => {
    const { container } = render(HomeView, { ...base(), spark: [1, 2, 3, 4, 5, 6] });
    expect(container.querySelectorAll('.spark i')).toHaveLength(6);
  });
});
