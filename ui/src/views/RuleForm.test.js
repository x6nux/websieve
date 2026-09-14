import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import RuleForm from './RuleForm.svelte';
import { ruleTypeToFormType } from '../lib/config-map.js';

/** 焦点陷阱的可聚焦元素查询，与组件里的那一份保持一致（与 SettingsOverlay.test.js 同一套） */
const focusablesIn = (dialog) => [
  ...dialog.querySelectorAll(
    'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])'
  ),
];

/**
 * 等打开时的初始聚焦落定（与 SettingsOverlay.test.js 同一套道理）。
 *
 * 组件把初始聚焦挂在 `tick().then()` 上，不等它就直接 `u.tab()` 的话，
 * 这个微任务会在 tab 的 await 中途兑现、抢走焦点，断言测到的就是
 * 「抢焦点的结果」而不是陷阱本身。
 */
const settleInitialFocus = (dialog) =>
  vi.waitFor(() => {
    if (!dialog.contains(document.activeElement)) throw new Error('初始聚焦还没落定');
  });

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

  it('匹配值仅含空白时同样不提交，报同样的错误 —— 证明 composeValue 的 .trim() 真的生效，不是只挡住了全空串这一种情形', async () => {
    const u = userEvent.setup();
    const p = base();
    render(RuleForm, p);
    await u.selectOptions(screen.getByLabelText('类型'), 'DOMAIN');
    await u.type(screen.getByLabelText('匹配值'), '   ');
    await u.click(screen.getByRole('button', { name: '保存' }));
    expect(p.onsubmit).not.toHaveBeenCalled();
    expect(screen.getByRole('alert')).toHaveTextContent('匹配值不能为空。');
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

  it('真正的调用链：parseRuleLine 短写先经 ruleTypeToFormType 翻译，再作为 initial.type 传入，类型下拉正确显示大写形式', () => {
    // 与上面「字段预填自 initial」那条不同：这里用的是 parseRuleLine 实际会
    // 产出的短写 'suffix'（不是凑巧大写化后能对上 TYPES 的 'domain-suffix'），
    // 锁定 parseRuleLine → ruleTypeToFormType → RuleForm.initial 这条链路，
    // 而不是只测 draftFrom 自己的大小写归一化。
    render(RuleForm, {
      ...base(),
      mode: 'edit',
      initial: {
        type: ruleTypeToFormType('suffix'),
        value: 'old.com',
        target: '香港节点',
        noResolve: false,
      },
    });
    expect(screen.getByLabelText('类型').value).toBe('DOMAIN-SUFFIX');
  });

  it('编辑模式下切换类型不会清空已经填好的匹配值/出站', async () => {
    // draftFrom 只在打开时执行一次；之后 draft.type 单纯由 bind:value 改写，
    // 没有别的 effect 会因为类型变化去清 value/target（唯一相关的 effect
    // 只清 noResolve）。这里把这条「不会互相清空」的行为钉住。
    const u = userEvent.setup();
    render(RuleForm, editProps());
    await u.selectOptions(screen.getByLabelText('类型'), 'DOMAIN-KEYWORD');
    expect(screen.getByLabelText('匹配值')).toHaveValue('old.com');
    expect(screen.getByLabelText('出站').value).toBe('香港节点');
  });
});

describe('规则表单 —— 对话框角色与焦点陷阱（与 SettingsOverlay 同一套骨架）', () => {
  it('是 modal dialog（role=dialog, aria-modal=true）', () => {
    render(RuleForm, base());
    const d = screen.getByRole('dialog');
    expect(d).toHaveAttribute('aria-modal', 'true');
    expect(d).toHaveAccessibleName(/新增规则/);
  });

  it('打开时焦点进入面板内，且落在第一个表单字段 #rf-type 上 —— 不是 header 里的 × 关闭按钮', async () => {
    render(RuleForm, base());
    const dialog = screen.getByRole('dialog');
    await settleInitialFocus(dialog);
    expect(dialog.contains(document.activeElement)).toBe(true);
    expect(document.activeElement.id).toBe('rf-type');
  });

  it('Tab 走到最后一个控件再 Tab 回到第一个（焦点陷阱）', async () => {
    const u = userEvent.setup();
    render(RuleForm, base());
    const dialog = screen.getByRole('dialog');
    const focusables = focusablesIn(dialog);
    expect(focusables.length).toBeGreaterThan(1);

    await settleInitialFocus(dialog);

    focusables[focusables.length - 1].focus();
    await u.tab();
    expect(document.activeElement).toBe(focusables[0]);
  });

  it('Shift+Tab 从第一个回到最后一个', async () => {
    const u = userEvent.setup();
    render(RuleForm, base());
    const dialog = screen.getByRole('dialog');
    const focusables = focusablesIn(dialog);

    await settleInitialFocus(dialog);

    focusables[0].focus();
    await u.tab({ shift: true });
    expect(document.activeElement).toBe(focusables[focusables.length - 1]);
  });
});
