import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import RuleForm from './RuleForm.svelte';

const base = () => ({
  open: true,
  mode: 'add',
  initial: null,
  outboundNames: ['日本节点', '香港节点'],
  groupNames: ['节点选择'],
  serverError: null,
  onsubmit: vi.fn(),
  onclose: vi.fn(),
});

describe('规则表单 —— 新增', () => {
  it('类型下拉有全部 7 种规则类型', () => {
    render(RuleForm, base());
    const opts = screen.getByLabelText('类型').querySelectorAll('option');
    const values = [...opts].map((o) => o.value);
    expect(values).toEqual([
      'DOMAIN', 'DOMAIN-SUFFIX', 'DOMAIN-KEYWORD', 'IP-CIDR', 'GEOSITE', 'GEOIP', 'MATCH',
    ]);
  });

  it('出站下拉包含出站名、组名与内置的 DIRECT/REJECT', () => {
    render(RuleForm, base());
    const opts = [...screen.getByLabelText('出站').querySelectorAll('option')].map((o) => o.value);
    expect(opts).toEqual(expect.arrayContaining(['日本节点', '香港节点', '节点选择', 'DIRECT', 'REJECT']));
  });

  it('类型为 MATCH 时不显示匹配值输入框', async () => {
    const u = userEvent.setup();
    render(RuleForm, base());
    await u.selectOptions(screen.getByLabelText('类型'), 'MATCH');
    expect(screen.queryByLabelText('匹配值')).not.toBeInTheDocument();
  });

  it('类型不是 IP-CIDR/GEOIP 时 no-resolve 复选框被禁用', () => {
    render(RuleForm, base());
    expect(screen.getByLabelText(/no-resolve/)).toBeDisabled();
  });

  it('切到 GEOIP 后 no-resolve 复选框可勾选', async () => {
    const u = userEvent.setup();
    render(RuleForm, base());
    await u.selectOptions(screen.getByLabelText('类型'), 'GEOIP');
    expect(screen.getByLabelText(/no-resolve/)).toBeEnabled();
  });

  it('提交时把字段拼成规则文本：TYPE,VALUE,TARGET', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.selectOptions(screen.getByLabelText('类型'), 'DOMAIN-SUFFIX');
    await u.type(screen.getByLabelText('匹配值'), 'example.com');
    await u.selectOptions(screen.getByLabelText('出站'), '日本节点');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith({ value: 'DOMAIN-SUFFIX,example.com,日本节点' });
  });

  it('MATCH 提交时拼成 TYPE,TARGET，没有中间的匹配值段', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.selectOptions(screen.getByLabelText('类型'), 'MATCH');
    await u.selectOptions(screen.getByLabelText('出站'), 'DIRECT');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith({ value: 'MATCH,DIRECT' });
  });

  it('勾选 no-resolve 后拼进第四段', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.selectOptions(screen.getByLabelText('类型'), 'IP-CIDR');
    await u.type(screen.getByLabelText('匹配值'), '10.0.0.0/8');
    await u.selectOptions(screen.getByLabelText('出站'), 'DIRECT');
    await u.click(screen.getByLabelText(/no-resolve/));
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).toHaveBeenCalledWith({ value: 'IP-CIDR,10.0.0.0/8,DIRECT,no-resolve' });
  });

  it('匹配值为空时不提交，本地报错，不调用 onsubmit', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.selectOptions(screen.getByLabelText('类型'), 'DOMAIN');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).not.toHaveBeenCalled();
    expect(screen.getByRole('alert')).toHaveTextContent(/匹配值/);
  });

  it('后端校验失败时 serverError 原样显示，不是前端猜测出来的措辞', () => {
    render(RuleForm, { ...base(), serverError: '"DOMAIN,,DIRECT" 不是一条合法规则：匹配值不能为空' });
    expect(screen.getByRole('alert')).toHaveTextContent('不是一条合法规则');
  });

  it('取消按钮调用 onclose，不调用 onsubmit', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.click(screen.getByRole('button', { name: '取消' }));
    expect(p.onclose).toHaveBeenCalled();
    expect(p.onsubmit).not.toHaveBeenCalled();
  });

  it('Escape 关闭', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.keyboard('{Escape}');
    expect(p.onclose).toHaveBeenCalled();
  });
});

describe('规则表单 —— 编辑', () => {
  const editProps = () => ({
    ...base(),
    mode: 'edit',
    initial: { type: 'domain-suffix', value: 'old.com', target: '香港节点', noResolve: false },
  });

  it('字段预填自 initial', () => {
    render(RuleForm, editProps());
    expect(screen.getByLabelText('类型').value).toBe('DOMAIN-SUFFIX');
    expect(screen.getByLabelText('匹配值').value).toBe('old.com');
    expect(screen.getByLabelText('出站').value).toBe('香港节点');
  });

  it('标题与新增区分，按钮文案保持保存', () => {
    render(RuleForm, editProps());
    expect(screen.getByRole('heading')).toHaveTextContent('编辑规则');
  });
});
