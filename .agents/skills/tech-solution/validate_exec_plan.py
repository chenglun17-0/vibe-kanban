#!/usr/bin/env python3
"""exec-plan 计划文档结构校验（结构级）。

用法:
    python3 validate_exec_plan.py <plan.md> [更多文件...]
    python3 validate_exec_plan.py --self-check

退出码: 0 全部通过；1 存在结构错误。
校验标准见同目录 SKILL.md「文档骨架」；取舍质量、验收命令真实性不在脚本范围。
"""
from __future__ import annotations

import re
import sys
import tempfile
from datetime import datetime
from pathlib import Path

STATUS_ENUM = {"草案", "进行中", "待核验", "已废弃", "已归档"}
REQUIRED_META = ["状态", "当前阶段", "最近更新", "关联变更"]
REQUIRED_SECTIONS = ["概述", "设计", "计划", "测试", "备注"]  # LeanSpec 五节式
OVERVIEW_ITEMS = ["非目标", "场景"]
PROD_ITEMS = ["兼容性", "失败行为", "可观测性", "上线与恢复"]
KEBAB_RE = re.compile(r"^[a-z0-9]+(-[a-z0-9]+)*\.md$")
LINK_RE = re.compile(r"\[[^\]]*\]\(([^)\s]+)\)")
MAX_LINES = 500  # 体量上限（LeanSpec Context Economy）；超出须拆成多份计划


def squash(s: str) -> str:
    """去空白后比较，容忍「目标 / 非目标」与「目标/非目标」。"""
    return re.sub(r"\s+", "", s)


def split_sections(lines: list[str]) -> list[tuple[str, list[str]]]:
    """按 ## 二级标题切分，返回 [(squash 后标题, 正文行)]。"""
    sections: list[tuple[str, list[str]]] = []
    title, body = None, []
    for line in lines:
        m = re.match(r"^##\s+(.+?)\s*$", line)
        if m:
            if title is not None:
                sections.append((title, body))
            title, body = squash(m.group(1)), []
        elif title is not None:
            body.append(line)
    if title is not None:
        sections.append((title, body))
    return sections


def validate_text(path: Path, text: str) -> list[str]:
    errors: list[str] = []
    parts = path.parts
    if "archive" in parts:
        return []  # 归档计划不校验
    if "exec-plan" not in parts or path.parent.name == "exec-plan":
        errors.append("路径必须为 docs/exec-plan/<业务模块>/<name>.md")
    if not KEBAB_RE.match(path.name):
        errors.append(f"文件名需短横线命名: {path.name}")

    lines = text.splitlines()
    if len(lines) > MAX_LINES:
        errors.append(f"超过 {MAX_LINES} 行（{len(lines)} 行），须拆成多份计划，主计划只留骨架和链接")

    # 元信息（首个二级标题之前）
    meta: dict[str, str] = {}
    for line in lines:
        if line.startswith("## "):
            break
        m = re.match(r"^-\s*(\S+?)\s*[:：]\s*(.*)$", line)
        if m:
            meta[m.group(1)] = m.group(2).strip()
    for key in REQUIRED_META:
        if not meta.get(key):
            errors.append(f"元信息缺失: {key}")
    status = meta.get("状态", "")
    if status and status not in STATUS_ENUM:
        errors.append(f"状态非法: {status}（可选: {'/'.join(sorted(STATUS_ENUM))}）")
    date = meta.get("最近更新", "")
    if date:
        try:
            datetime.strptime(date, "%Y-%m-%d")
        except ValueError:
            errors.append(f"最近更新需为 YYYY-MM-DD: {date}")

    # 必填章节
    sections = split_sections(lines)
    bodies = dict(sections)
    for req in REQUIRED_SECTIONS:
        if squash(req) not in bodies:
            errors.append(f"缺少必填章节: ## {req}")

    # 概述：非目标 + 验收场景
    body = bodies.get("概述")
    if body is not None:
        joined = "\n".join(body)
        for item in OVERVIEW_ITEMS:
            if item not in joined:
                errors.append(f"概述缺少「{item}」（需含目标/非目标与每需求的验收场景）")

    # 设计：生产环境四项
    body = bodies.get("设计")
    if body is not None:
        joined = "\n".join(body)
        for item in PROD_ITEMS:
            if item not in joined:
                errors.append(f"设计缺少「{item}」（生产环境检查；不适用写 N/A + 原因）")

    # 计划：每个 ### Phase 块含 验收命令 / 预期证据
    body = bodies.get("计划")
    if body is not None:
        chunks: list[list[str]] = []
        cur = None
        for line in body:
            if re.match(r"^###\s", line):
                cur = [line]
                chunks.append(cur)
            elif cur is not None:
                cur.append(line)
        phases = [c for c in chunks if re.match(r"^###\s+Phase\b", c[0])]
        if not phases:
            errors.append("计划缺少 ### Phase 阶段块")
        for c in phases:
            joined = "\n".join(c)
            name = c[0].lstrip("# ").strip()
            for field in ("验收命令", "预期证据"):
                if field not in joined:
                    errors.append(f"{name} 缺少「{field}」")

    # 相对链接必须存在（计划新增路径不要做成链接，用行内代码并标注 新增）
    in_fence = False
    for i, line in enumerate(lines, 1):
        if line.strip().startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue
        for m in LINK_RE.finditer(line):
            target = m.group(1).split("#", 1)[0]
            if not target or "://" in target or target.startswith(("mailto:", "/", "~")):
                continue
            if not (path.parent / target).resolve().exists():
                errors.append(f"第 {i} 行死链: {m.group(1)}")
        # openspec 已废弃：禁止 markdown 链接或「关联变更」元信息把 openspec 当活路径引用
        # （grep 命令、Git 历史/删除前/已废弃/外部仓库等显式标注的引用放行；正文真源声明由评审负责）
        _hist = any(k in line for k in ("Git 历史", "git 历史", "删除前", "已废弃", "不再", "历史路径", "hlzs_web", "外部仓库"))
        _link_to_openspec = any("openspec/" in m.group(1).split("#", 1)[0] for m in LINK_RE.finditer(line))
        _path = "openspec/changes" in line or "openspec/specs" in line
        _assoc = line.lstrip().startswith("- 关联变更")
        if (_link_to_openspec or (_path and _assoc)) and not _hist:
            errors.append(f"第 {i} 行引用 openspec 活路径（openspec 已废弃；`关联变更` 不写 openspec change-id，历史证据改指 Git 历史）")
    return errors


def validate(path: Path) -> list[str]:
    return validate_text(path, path.read_text(encoding="utf-8"))


def demo() -> None:
    valid = """# 演示计划

- 状态：草案
- 当前阶段：方案待评审
- 最近更新：2026-01-01
- 关联变更：无

## 概述

问题：x。目标：y。非目标：z。

- 需求 1：…… 场景：……

## 设计

直接改 x.py。

- 兼容性：N/A，无存量数据
- 失败行为：超时即失败
- 可观测性：N/A，无新增指标
- 上线与恢复：直接发布，回滚即 revert

## 计划

### Phase 1 - 落地

- 改动：x.py
- 工作目录：`backend/`
- 验收命令：`pytest -v`
- 预期证据：全部通过

## 测试

`python3 x.py --self-check` 通过。

## 备注

无。
"""
    with tempfile.TemporaryDirectory() as d:
        p = Path(d) / "docs" / "exec-plan" / "chat" / "demo-plan.md"
        p.parent.mkdir(parents=True)
        assert validate_text(p, valid) == [], "合法文档应通过"
        broken = valid.replace("## 概述", "## 背景").replace("- 状态：草案", "- 状态：未知")
        errs = validate_text(p, broken)
        assert any("概述" in e for e in errs), "缺章节应被拦截"
        assert any("状态非法" in e for e in errs), "非法状态应被拦截"
        no_phase = valid.replace("### Phase 1 - 落地", "### 落地")
        assert any("Phase" in e for e in validate_text(p, no_phase)), "缺 Phase 块应被拦截"
        long_doc = valid + "\n填充\n" * 600
        assert any("500" in e for e in validate_text(p, long_doc)), "超长应被拦截"
        (p.parent / "detail.md").write_text("# d\n", encoding="utf-8")
        assert validate_text(p, valid + "\n[详情](detail.md)\n") == [], "存在的链接应通过"
        dead = valid + "\n[缺失](missing.md)\n"
        assert any("死链" in e for e in validate_text(p, dead)), "死链应被拦截"
        openspec_violation = valid.replace("- 关联变更：无", "- 关联变更：`openspec/changes/add-x`")
        assert any("openspec 活路径" in e for e in validate_text(p, openspec_violation)), "关联变更 引用 openspec 应被拦截"
        openspec_link = valid + "\n见 [proposal](../../../openspec/changes/add-x/proposal.md)\n"
        assert any("openspec 活路径" in e for e in validate_text(p, openspec_link)), "openspec markdown 链接应被拦截"
        openspec_hist = valid + "\n历史证据见 Git 历史（openspec/changes/add-x/evidence/ 删除前）\n"
        assert validate_text(p, openspec_hist) == [], "标注 Git 历史的 openspec 引用应放行"
    print("self-check ok")


def main(argv: list[str]) -> int:
    if "--self-check" in argv:
        demo()
        return 0
    if not argv:
        print(__doc__)
        return 1
    failed = False
    for name in argv:
        path = Path(name)
        if not path.is_file():
            print(f"✗ {name}: 文件不存在")
            failed = True
            continue
        errors = validate(path)
        if errors:
            failed = True
            for e in errors:
                print(f"✗ {name}: {e}")
        else:
            print(f"✓ {name}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
