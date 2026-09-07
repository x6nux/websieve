import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import ProxyForm from './ProxyForm.svelte';

const base = () => ({
  open: true,
  serverError: null,
  onsubmit: vi.fn(),
  onclose: vi.fn(),
});

describe('节点新增表单', () => {
  it('私钥输入框是 password 类型', () => {
    render(ProxyForm, base());
    expect(screen.getByLabelText('client-priv')).toHaveAttribute('type', 'password');
  });

  it('私钥输入框禁止浏览器/WebView2 自动填充与保存密码提示', () => {
    render(ProxyForm, base());
    expect(screen.getByLabelText('client-priv')).toHaveAttribute('autocomplete', 'new-password');
  });

  it('展示与 SettingsOverlay 导出提示同源的安全提示语', () => {
    render(ProxyForm, base());
    expect(screen.getByText(/私钥仅受文件系统权限保护/)).toBeInTheDocument();
  });

  it('必填字段为空时不提交，本地报错', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).not.toHaveBeenCalled();
    expect(screen.getByRole('alert')).toBeInTheDocument();
  });

  it('client-priv 仅含空白时视为未填，不提交', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.type(screen.getByLabelText('名称'), '日本节点');
    await u.type(screen.getByLabelText('地址（url）'), 'https://example.com/');
    await u.type(screen.getByLabelText('server-pub'), 'aa==');
    await u.type(screen.getByLabelText('client-priv'), '   ');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).not.toHaveBeenCalled();
    expect(screen.getByRole('alert')).toBeInTheDocument();
  });

  it('client-priv 前后空白在提交时被裁剪', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.type(screen.getByLabelText('名称'), '日本节点');
    await u.type(screen.getByLabelText('地址（url）'), 'https://example.com/');
    await u.type(screen.getByLabelText('server-pub'), 'aa==');
    await u.type(screen.getByLabelText('client-priv'), '  bb==  ');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith(
      expect.arrayContaining(['  client-priv: "bb=="']),
    );
  });

  it('填完必填字段提交时拼出固定格式的行数组', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.type(screen.getByLabelText('名称'), '日本节点');
    await u.type(screen.getByLabelText('地址（url）'), 'https://example.com/');
    await u.type(screen.getByLabelText('server-pub'), 'aa==');
    await u.type(screen.getByLabelText('client-priv'), 'bb==');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith([
      '- name: "日本节点"',
      '  type: websieve',
      '  url: https://example.com/',
      '  server-pub: "aa=="',
      '  client-priv: "bb=="',
    ]);
  });

  it('提交后立刻清空私钥输入框，不留在 DOM 里', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.type(screen.getByLabelText('名称'), 'x');
    await u.type(screen.getByLabelText('地址（url）'), 'https://x/');
    await u.type(screen.getByLabelText('server-pub'), 'aa');
    await u.type(screen.getByLabelText('client-priv'), 'bb');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(screen.getByLabelText('client-priv').value).toBe('');
  });

  it('取消时也清空私钥输入框', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.type(screen.getByLabelText('client-priv'), 'bb');
    await u.click(screen.getByRole('button', { name: '取消' }));
    expect(screen.getByLabelText('client-priv').value).toBe('');
  });

  it('展开「高级」后可填 extra-sessions，留空则不出现在提交的行里', async () => {
    const u = userEvent.setup();
    const p = base();
    render(ProxyForm, p);
    await u.type(screen.getByLabelText('名称'), '日本节点');
    await u.type(screen.getByLabelText('地址（url）'), 'https://example.com/');
    await u.type(screen.getByLabelText('server-pub'), 'aa');
    await u.type(screen.getByLabelText('client-priv'), 'bb');
    await u.click(screen.getByRole('button', { name: '高级' }));
    await u.type(screen.getByLabelText('extra-sessions'), '4');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith(
      expect.arrayContaining(['  extra-sessions: 4']),
    );
  });

  it('后端报错（如名字冲突）原样显示', () => {
    render(ProxyForm, { ...base(), serverError: '名字 "日本节点" 已经被一个出站或代理组占用，换一个名字' });
    expect(screen.getByRole('alert')).toHaveTextContent('已经被一个出站或代理组占用');
  });
});
