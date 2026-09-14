#!/usr/bin/env python3
"""把 macOS `sample` 的调用树转成折叠栈和火焰图 SVG。

为什么不用 samply/Instruments：`samply record -p` 要 `task_for_pid` 权限，
需要跑一次交互式的 `samply setup` 授权；`sample` 是系统自带、不需要任何授权，
拿到的调用树信息量对定位热点已经够了。

`sample` 的缩进不是空格，而是 `+ ! : |` 这套画树线的字符，所以深度只能按
"数字前的前缀长度"来判，不能按空格数。

用法：
    flame.py <sample输出> [-o out.svg] [--collapsed out.txt] [--min-pct 0.1]

默认会把纯等待的栈（线程停在内核里睡觉）单独统计并从火焰图里剔除——
它们在 `sample` 里和真正烧 CPU 的栈长得一模一样，混在一起会让火焰图 90%
的面积都是"什么都没干"，反而看不见真正的热点。
"""
import re
import sys
import collections

# 叶子落在这些符号上 = 线程在内核里等，不是在算。
IDLE_LEAVES = {
    "__psynch_cvwait", "__workq_kernreturn", "mach_msg2_trap", "kevent",
    "__psynch_mutexwait", "__semwait_signal", "poll", "select", "__accept",
    "read", "__read_nocancel", "swtch_pri", "thread_switch",
}

ROW = re.compile(r"^([^0-9]*?)(\d+)\s+(.+)$")


def parse(path):
    """返回 [(depth, count, symbol, lib)]，按文件顺序。"""
    rows = []
    for line in open(path, errors="ignore"):
        line = line.rstrip("\n")
        m = ROW.match(line)
        if not m:
            continue
        prefix, cnt, rest = m.group(1), int(m.group(2)), m.group(3)
        # 前缀里除了树线字符不该有别的；有的话说明这行不是调用树。
        if prefix.strip(" +!:|"):
            continue
        sym = re.sub(r"\s+\[0x[0-9a-f]+\]\s*$", "", rest)
        sym = re.sub(r"\s+\+\s+\d+\s*$", "", sym)
        lib = "?"
        m2 = re.search(r"\(in ([^)]+)\)", sym)
        if m2:
            lib = m2.group(1).split("/")[-1]
            sym = sym[: m2.start()].strip()
        rows.append((len(prefix), cnt, sym, lib))
    return rows


def collapse(rows):
    """展开成 {分号分隔的栈: 自身样本数}。"""
    stacks = collections.Counter()
    path = []  # [(depth, symbol)]
    for i, (d, cnt, sym, _lib) in enumerate(rows):
        while path and path[-1][0] >= d:
            path.pop()
        path.append((d, sym))
        # 自身样本 = 本节点计数 − 直接子节点计数之和
        kid_depth, kids = None, 0
        for j in range(i + 1, len(rows)):
            dj = rows[j][0]
            if dj <= d:
                break
            if kid_depth is None:
                kid_depth = dj
            if dj == kid_depth:
                kids += rows[j][1]
        self_n = cnt - kids
        if self_n > 0:
            stacks[";".join(s for _, s in path)] += self_n
    return stacks


def split_idle(stacks):
    """拆成 (干活的, 等待的)。判据是叶子符号。"""
    busy, idle = collections.Counter(), collections.Counter()
    for st, n in stacks.items():
        leaf = st.rsplit(";", 1)[-1]
        (idle if leaf in IDLE_LEAVES else busy)[st] += n
    return busy, idle


PALETTE = ["#e8654a", "#e8874a", "#e8a64a", "#d9c04a", "#c2cf52"]


def svg(stacks, out, title, width=1600, row_h=17, min_pct=0.05):
    """最小可用的火焰图 SVG：矩形 + 悬停提示，不依赖任何外部工具。"""
    total = sum(stacks.values()) or 1
    # 建树
    root = {"name": title, "value": 0, "children": {}}
    for st, n in stacks.items():
        node = root
        node["value"] += n
        for part in st.split(";"):
            node = node["children"].setdefault(
                part, {"name": part, "value": 0, "children": {}}
            )
            node["value"] += n
    boxes = []

    def walk(node, depth, x0):
        w = node["value"] * width / total
        if w >= width * min_pct / 100:
            boxes.append((x0, depth, w, node["name"], node["value"]))
        cx = x0
        for kid in sorted(node["children"].values(), key=lambda k: -k["value"]):
            walk(kid, depth + 1, cx)
            cx += kid["value"] * width / total

    walk(root, 0, 0)
    depth_max = max(b[1] for b in boxes) + 1
    h = depth_max * row_h + 40
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{h}" '
        f'font-family="Menlo,monospace" font-size="11">',
        f'<rect width="{width}" height="{h}" fill="#fff"/>',
    ]
    for x, d, w, name, val in boxes:
        y = h - (d + 1) * row_h - 5
        pct = val * 100 / total
        color = PALETTE[hash(name) % len(PALETTE)]
        label = name if w > len(name) * 6 else (name[: max(0, int(w / 6) - 2)] + "…" if w > 24 else "")
        esc = lambda s: s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
        parts.append(
            f'<g><title>{esc(name)} — {val} 样本 ({pct:.2f}%)</title>'
            f'<rect x="{x:.1f}" y="{y}" width="{max(w-1,0.5):.1f}" height="{row_h-1}" '
            f'fill="{color}" stroke="#fff" stroke-width="0.5"/>'
            f'<text x="{x+3:.1f}" y="{y+row_h-6}">{esc(label)}</text></g>'
        )
    parts.append("</svg>")
    open(out, "w").write("\n".join(parts))


if __name__ == "__main__":
    import argparse

    ap = argparse.ArgumentParser()
    ap.add_argument("input")
    ap.add_argument("-o", "--out", default="/tmp/flame.svg")
    ap.add_argument("--collapsed")
    ap.add_argument("--min-pct", type=float, default=0.05)
    ap.add_argument("--top", type=int, default=20)
    a = ap.parse_args()

    rows = parse(a.input)
    stacks = collapse(rows)
    busy, idle = split_idle(stacks)
    tb, ti = sum(busy.values()), sum(idle.values())
    print(f"样本合计 {tb+ti}：真正占用 CPU {tb}（{tb*100/(tb+ti):.1f}%），"
          f"内核等待 {ti}（{ti*100/(tb+ti):.1f}%）")
    print(f"\n=== 自身样本 Top {a.top}（只算干活的，占忙碌样本的比例）===")
    leaves = collections.Counter()
    for st, n in busy.items():
        leaves[st.rsplit(";", 1)[-1]] += n
    for sym, n in leaves.most_common(a.top):
        print(f"{n*100/max(tb,1):6.2f}%  {n:6d}  {sym[:88]}")
    if a.collapsed:
        with open(a.collapsed, "w") as f:
            for st, n in busy.most_common():
                f.write(f"{st} {n}\n")
        print(f"\n折叠栈已写入 {a.collapsed}")
    svg(busy, a.out, "CPU（已剔除内核等待）", min_pct=a.min_pct)
    print(f"火焰图已写入 {a.out}")
