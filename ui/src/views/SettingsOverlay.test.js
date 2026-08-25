import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import SettingsOverlay from './SettingsOverlay.svelte';

const config = {
  mixedPort: 7890,
  allowLan: false,
  mode: 'rule',
  systemProxy: false,
  logLevel: 'info',
  geoAutoUpdate: true,
  carrier: 'shared',
};
const base = () => ({ open: true, config, onclose: vi.fn(), onsave: vi.fn(), onexport: vi.fn() });

/** 焦点陷阱的可聚焦元素查询，与组件里的那一份保持一致 */
const focusablesIn = (dialog) => [
  ...dialog.querySelectorAll(
    'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])'
  ),
];

/**
 * 等打开时的初始聚焦落定。
 *
 * 组件把初始聚焦挂在 `tick().then()` 上（DOM 得先存在才能聚焦）。
 * 不等它就直接 `u.tab()` 的话，这个微任务会在 tab 的 await 中途兑现、
 * 把焦点抢到第一个控件上，于是断言测到的是「抢焦点的结果」而不是
 * 陷阱本身 —— 一个会稳定通过或稳定失败、但两种情况都没测到目标的测试。
 */
const settleInitialFocus = (dialog) =>
  vi.waitFor(() => {
    if (!dialog.contains(document.activeElement)) throw new Error('初始聚焦还没落定');
  });

describe('设置覆盖层', () => {
  it('是 modal dialog 而非第四个标签（spec §11.3）', () => {
    render(SettingsOverlay, base());
    const d = screen.getByRole('dialog');
    expect(d).toHaveAttribute('aria-modal', 'true');
    expect(d).toHaveAccessibleName(/设置/);
  });

  it('打开时焦点进入层内', async () => {
    render(SettingsOverlay, base());
    await vi.waitFor(() => {
      expect(screen.getByRole('dialog').contains(document.activeElement)).toBe(true);
    });
  });

  it('Esc 关闭', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    await u.keyboard('{Escape}');
    expect(p.onclose).toHaveBeenCalled();
  });

  it('关闭按钮有可访问名字', () => {
    render(SettingsOverlay, base());
    expect(screen.getByRole('button', { name: /关闭/ })).toBeInTheDocument();
  });

  it('open=false 时不渲染', () => {
    render(SettingsOverlay, { ...base(), open: false });
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('每个表单控件都有关联的 label', () => {
    render(SettingsOverlay, base());
    for (const el of [
      screen.getByLabelText(/混合端口/),
      screen.getByLabelText(/允许局域网/),
      screen.getByLabelText(/系统代理/),
    ])
      expect(el).toBeInTheDocument();
  });

  it('端口非法时给出错误且不静默吞掉', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    const port = screen.getByLabelText(/混合端口/);
    await u.clear(port);
    await u.type(port, '99999');
    await u.click(screen.getByRole('button', { name: /保存/ }));
    expect(screen.getByRole('alert')).toBeInTheDocument();
    expect(p.onsave).not.toHaveBeenCalled();
  });

  it('导出入口显式警告含私钥（spec §5.4）', () => {
    render(SettingsOverlay, base());
    expect(screen.getByText(/私钥/)).toBeInTheDocument();
  });

  it('保存提示会丢失非规则区注释（spec §5.6）', () => {
    render(SettingsOverlay, base());
    // getAllBy 而非 getBy：提示里「注释会丢失」被 <b> 包着强调，
    // testing-library 会把 <p> 与 <b> 都算成匹配节点。断言的是
    // 「这句话在」，不是「页面上只有一处提到注释」。
    expect(screen.getAllByText(/注释/).length).toBeGreaterThan(0);
    // 两件事都要说到：非规则区会丢，规则区不会 —— 只说前半句会让用户
    // 以为排序也会毁掉他的注释，于是再也不敢用排序
    const body = screen.getByRole('dialog').textContent;
    expect(body).toMatch(/注释会丢失/);
    expect(body).toMatch(/规则区.*保留/);
  });

  it('合法输入能保存', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    await u.click(screen.getByLabelText(/允许局域网/));
    await u.click(screen.getByRole('button', { name: /保存/ }));
    expect(p.onsave).toHaveBeenCalled();
  });
});

/**
 * 私钥不进这一层。
 *
 * `config_get` 递归脱敏 `client-priv`，`config_get_raw` 逐字返回**明文私钥**。
 * 本组件的契约是「只吃脱敏过的那一份」—— 它从不自己发起 IPC，
 * 导出走 `onexport` 交给父组件，由父组件在用户显式点击时才去取原文。
 *
 * 下面这条守的是「将来有人给设置层加一个『显示全部配置字段』的循环」——
 * 那一刻这条测试变红，而不是等到私钥进了某份崩溃报告才被发现。
 */
describe('设置覆盖层 · 私钥边界', () => {
  const SECRET = 'AAAA-this-would-be-a-real-private-key-AAAA';

  it('即使调用方误传了明文私钥，也不渲染到 DOM 里', () => {
    const { container } = render(SettingsOverlay, {
      ...base(),
      config: { ...config, 'client-priv': SECRET, clientPriv: SECRET },
    });
    expect(container.textContent).not.toContain(SECRET);
    // input 的 value 不进 textContent，得单独查一遍
    for (const el of container.querySelectorAll('input, textarea')) {
      expect(el.value).not.toContain(SECRET);
    }
  });

  it('导出是显式动作，不在挂载时自动触发', () => {
    const p = base();
    render(SettingsOverlay, p);
    expect(p.onexport).not.toHaveBeenCalled();
  });

  it('点导出才回调，且旁边就是私钥警告', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    await u.click(screen.getByRole('button', { name: /导出/ }));
    expect(p.onexport).toHaveBeenCalled();
    expect(screen.getByText(/私钥/)).toBeInTheDocument();
  });
});

describe('设置覆盖层 · 草稿与焦点', () => {
  it('父组件重渲染不会冲掉用户正在编辑的内容', async () => {
    const u = userEvent.setup();
    const { rerender } = render(SettingsOverlay, base());
    const port = screen.getByLabelText(/混合端口/);
    await u.clear(port);
    await u.type(port, '1080');
    // 父组件因为别的原因重新传了一份等值但不同引用的 config
    await rerender({ ...base(), config: { ...config } });
    expect(screen.getByLabelText(/混合端口/)).toHaveValue(1080);
  });

  it('重新打开时草稿回到当前配置，不留上一次的未保存改动', async () => {
    const u = userEvent.setup();
    const { rerender } = render(SettingsOverlay, base());
    const port = screen.getByLabelText(/混合端口/);
    await u.clear(port);
    await u.type(port, '1080');
    await rerender({ ...base(), open: false });
    await rerender(base());
    expect(screen.getByLabelText(/混合端口/)).toHaveValue(7890);
  });

  it('改正端口后错误消失且能保存', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    const port = screen.getByLabelText(/混合端口/);
    await u.clear(port);
    await u.type(port, '99999');
    await u.click(screen.getByRole('button', { name: /保存/ }));
    expect(screen.getByRole('alert')).toBeInTheDocument();

    await u.clear(port);
    await u.type(port, '1080');
    await u.click(screen.getByRole('button', { name: /保存/ }));
    expect(screen.queryByRole('alert')).toBeNull();
    expect(p.onsave).toHaveBeenCalledWith(expect.objectContaining({ mixedPort: 1080 }));
  });

  it('保存回传的是数字而非字符串 —— 端口进 YAML 得是标量', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    await u.click(screen.getByRole('button', { name: /保存/ }));
    const arg = p.onsave.mock.calls[0][0];
    expect(typeof arg.mixedPort).toBe('number');
  });

  it('端口为空时报错而不是当成 0', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    await u.clear(screen.getByLabelText(/混合端口/));
    await u.click(screen.getByRole('button', { name: /保存/ }));
    expect(screen.getByRole('alert')).toBeInTheDocument();
    expect(p.onsave).not.toHaveBeenCalled();
  });

  it('取消不保存', async () => {
    const u = userEvent.setup();
    const p = base();
    render(SettingsOverlay, p);
    await u.click(screen.getByRole('button', { name: /取消/ }));
    expect(p.onclose).toHaveBeenCalled();
    expect(p.onsave).not.toHaveBeenCalled();
  });

  it('Tab 走到最后一个控件再 Tab 回到第一个（焦点陷阱）', async () => {
    const u = userEvent.setup();
    render(SettingsOverlay, base());
    const dialog = screen.getByRole('dialog');
    const focusables = focusablesIn(dialog);
    expect(focusables.length).toBeGreaterThan(1);

    // 必须先等打开时的初始聚焦落定。它挂在 tick().then() 上，
    // 若不等，它会在 u.tab() 的 await 中途抢过焦点，测出来的就不是陷阱本身
    await settleInitialFocus(dialog);

    focusables[focusables.length - 1].focus();
    await u.tab();
    expect(document.activeElement).toBe(focusables[0]);
  });

  it('Shift+Tab 从第一个回到最后一个', async () => {
    const u = userEvent.setup();
    render(SettingsOverlay, base());
    const dialog = screen.getByRole('dialog');
    const focusables = focusablesIn(dialog);

    await settleInitialFocus(dialog);

    focusables[0].focus();
    await u.tab({ shift: true });
    expect(document.activeElement).toBe(focusables[focusables.length - 1]);
  });
});
