import { describe, it, expect } from 'vitest';
import { FlowStore, hostOf, WEIGHT_BYTES, WEIGHT_CONNS } from './flows.js';

const open = (id, target, outbound) => ({ id, target, outbound, state: 'open' });

describe('hostOf', () => {
  it('剥掉端口', () => {
    expect(hostOf('example.com:443')).toBe('example.com');
    expect(hostOf('1.2.3.4:80')).toBe('1.2.3.4');
  });
  it('IPv6 字面量不被冒号误伤', () => {
    expect(hostOf('[2001:db8::1]:443')).toBe('2001:db8::1');
  });
  it('没有端口时原样返回', () => {
    expect(hostOf('example.com')).toBe('example.com');
  });
  it('空输入不抛错', () => {
    expect(hostOf('')).toBe('');
    expect(hostOf(undefined)).toBe('');
  });
});

describe('FlowStore', () => {
  it('把连接按 (站点, 规则, 出站) 聚成流', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP'), open(2, 'a.com:443', 'JP'), open(3, 'b.com:443', 'DIRECT')]);
    const rows = s.rows();
    expect(rows).toHaveLength(2);
    const a = rows.find((r) => r.site === 'a.com');
    expect(a.conns).toBe(2);
    expect(a.outbound).toBe('JP');
  });

  it('同一站点走不同出站算两条流 —— 去向是流的身份', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP'), open(2, 'a.com:443', 'SG')]);
    expect(s.rows()).toHaveLength(2);
  });

  it('close 不减少累计连接数 —— 这是累计量而非当前量', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    s.apply([{ id: 1, target: 'a.com:443', outbound: 'JP', state: 'close' }]);
    expect(s.rows()[0].conns).toBe(1);
  });

  it('reject 归到 REJECT 出站', () => {
    const s = new FlowStore();
    s.apply([{ id: 1, target: 'ads.com:443', outbound: 'REJECT', state: 'reject' }]);
    expect(s.rows()[0].outbound).toBe('REJECT');
  });

  it('同一 id 重复 open 不重复计数', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    s.apply([open(1, 'a.com:443', 'JP')]);
    expect(s.rows()[0].conns).toBe(1);
  });

  it('规则未知时用占位符而非留空 —— 桑基图中间层不能有空节点', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    expect(s.rows()[0].rule).toBeTruthy();
  });

  it('已知规则时带上（阶段 2 补齐 rule 字段后自动生效）', () => {
    const s = new FlowStore();
    s.apply([{ ...open(1, 'a.com:443', 'JP'), rule: 'geosite cn' }]);
    expect(s.rows()[0].rule).toBe('geosite cn');
  });

  it('bytes 字段存在但在无字节数据时为 0', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    expect(s.rows()[0].bytes).toBe(0);
    expect(s.hasBytes()).toBe(false);
  });

  it('事件带 bytes 时如实累加并翻转 hasBytes', () => {
    const s = new FlowStore();
    s.apply([{ ...open(1, 'a.com:443', 'JP'), bytes: 1024 }]);
    expect(s.rows()[0].bytes).toBe(1024);
    expect(s.hasBytes()).toBe(true);
  });

  it('有上限，不会无限增长', () => {
    const s = new FlowStore(50);
    for (let i = 0; i < 200; i++) s.apply([open(i, `s${i}.com:443`, 'JP')]);
    expect(s.rows().length).toBeLessThanOrEqual(50);
  });

  it('触顶时保留连接数最多的，而非最先到的', () => {
    const s = new FlowStore(2);
    s.apply([open(1, 'hot.com:443', 'JP'), open(2, 'hot.com:443', 'JP'), open(3, 'hot.com:443', 'JP')]);
    s.apply([open(4, 'cold1.com:443', 'JP'), open(5, 'cold2.com:443', 'JP')]);
    expect(s.rows().some((r) => r.site === 'hot.com')).toBe(true);
  });

  it('reset 清空', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    s.reset();
    expect(s.rows()).toHaveLength(0);
  });
});

// ── 计量单位的诚实性 ────────────────────────────────────────────
//
// 这一组守的是整个模块存在的前提：ConnectionDelta 现在没有字节数，
// 所以「粗细」画的是连接数。单位必须**跟着数值一起走**，而不是只写在
// 注释里 —— 注释拦不住下游把 weight 当字节渲染。
describe('weight 与单位', () => {
  it('无字节数据时 weight 取连接数，单位如实标注为 conns', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP'), open(2, 'a.com:443', 'JP')]);
    const r = s.rows()[0];
    expect(r.weight).toBe(2);
    expect(r.weightUnit).toBe(WEIGHT_CONNS);
    expect(s.weightUnit()).toBe(WEIGHT_CONNS);
  });

  it('有字节数据时 weight 取字节，单位翻转为 bytes', () => {
    const s = new FlowStore();
    s.apply([{ ...open(1, 'a.com:443', 'JP'), bytes: 2048 }]);
    const r = s.rows()[0];
    expect(r.weight).toBe(2048);
    expect(r.weightUnit).toBe(WEIGHT_BYTES);
    expect(s.weightUnit()).toBe(WEIGHT_BYTES);
  });

  it('每一行都带着单位 —— 下游拿到 weight 就必然拿到它的含义', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP'), open(2, 'b.com:443', 'SG')]);
    for (const r of s.rows()) expect(r.weightUnit).toBe(WEIGHT_CONNS);
  });

  it('单位一翻转，此前累积的行也跟着按字节计 —— 不会图上一半字节一半连接数', () => {
    const s = new FlowStore();
    s.apply([open(1, 'a.com:443', 'JP')]);
    expect(s.rows()[0].weightUnit).toBe(WEIGHT_CONNS);
    s.apply([{ ...open(2, 'b.com:443', 'SG'), bytes: 512 }]);
    for (const r of s.rows()) expect(r.weightUnit).toBe(WEIGHT_BYTES);
  });

  it('reset 把单位也退回连接数 —— 否则换配置后会拿旧结论画新数据', () => {
    const s = new FlowStore();
    s.apply([{ ...open(1, 'a.com:443', 'JP'), bytes: 512 }]);
    s.reset();
    expect(s.weightUnit()).toBe(WEIGHT_CONNS);
  });
});

describe('聚合键的分隔', () => {
  it('站点与规则的边界不会串味 —— 分隔符必须真的分隔', () => {
    // ('ab','c') 与 ('a','bc') 在无分隔符拼接下都是 'abc'，会被错并成一条流。
    // 域名与规则值里都不可能出现 NUL，所以它是安全的分隔符。
    const s = new FlowStore();
    s.apply([
      { ...open(1, 'ab.com:443', 'JP'), rule: 'c' },
      { ...open(2, 'ab.co:443', 'JP'), rule: 'mc' },
    ]);
    expect(s.rows()).toHaveLength(2);
  });
});
