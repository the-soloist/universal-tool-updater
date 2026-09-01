#!/usr/bin/env python3
"""通用语义化版本 bump 工具。

扫描项目中常见的版本文件，按 semver 规范递增版本号。
支持 --plugin 按名称定向 bump 单个插件。

用法:
  python bump.py list                        # 列出所有版本号
  python bump.py patch                       # 全部 patch
  python bump.py patch --plugin mineru       # 只 bump mineru
  python bump.py minor --dry-run             # 预览
  python bump.py patch --file package.json   # 指定文件
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

VERSION_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")

DEFAULT_PATTERNS = [
    "VERSION",
    "version.json",
    ".claude-plugin/marketplace.json",
    "**/.claude-plugin/plugin.json",
    "**/package.json",
    "**/pyproject.toml",
    "**/Cargo.toml",
    "**/VERSION",
]

# 扫描时跳过的目录（依赖、构建产物等，避免把第三方版本号也扫进来）
EXCLUDE_DIRS = {
    "node_modules", ".venv", "venv", "env", "site-packages",
    ".git", "dist", "build", ".tox", "__pycache__", ".mypy_cache",
}

VERSION_EXTRACTORS = {
    ".json": "json",
    ".toml": "toml",
}

# TOML 中权威版本号所在的 table：PEP 621 [project]、Poetry [tool.poetry]、Cargo [package]。
# None（顶层、无表头）也视为权威，兼容简单的 version = "x" 文件。
TOML_VERSION_TABLES = ("project", "tool.poetry", "package")
TOML_HEADER_RE = re.compile(r"^\s*\[+\s*([^\]#]+?)\s*\]+")
TOML_VERSION_RE = re.compile(r"""^\s*version\s*=\s*['"]([^'"]+)['"]""")


def parse_version(v: str) -> tuple[int, int, int] | None:
    m = VERSION_RE.match(v.strip())
    if not m:
        return None
    return int(m.group(1)), int(m.group(2)), int(m.group(3))


def bump_version(major: int, minor: int, patch: int, bump_type: str) -> str:
    if bump_type == "major":
        return f"{major + 1}.0.0"
    elif bump_type == "minor":
        return f"{major}.{minor + 1}.0"
    else:
        return f"{major}.{minor}.{patch + 1}"


def find_files(root: Path, patterns: list[str]) -> list[Path]:
    seen = set()
    result = []
    for pattern in patterns:
        if "**" in pattern:
            matches = root.glob(pattern)
        else:
            matches = [root / pattern]
        for f in sorted(matches):
            if f.is_file() and not (EXCLUDE_DIRS & set(f.parts)):
                resolved = f.resolve()
                if resolved not in seen:
                    seen.add(resolved)
                    result.append(f)
    return result


# ── 版本提取 ─────────────────────────────────────────────────

class VersionEntry:
    """一个可定位的版本号条目。"""

    def __init__(
        self,
        filepath: Path,
        label: str,
        version: str,
        kind: str,
        plugin_name: str | None = None,
        section: str | None = None,
    ):
        self.filepath = filepath
        self.label = label
        self.version = version
        self.kind = kind          # json / toml / plain
        self.plugin_name = plugin_name
        self.section = section    # TOML 所在 table（project / tool.poetry / package / None）

    def matches(self, plugin_filter: str | None) -> bool:
        if not plugin_filter:
            return True
        return self.plugin_name == plugin_filter


def _iter_toml_lines(text: str):
    """逐行扫描 TOML，跟踪当前 table。

    yield (line, table, match)：table 为当前表头名（顶层为 None），
    match 为该行匹配到的 version 正则（无则 None）。
    """
    table = None
    for line in text.splitlines(keepends=True):
        header = TOML_HEADER_RE.match(line)
        if header:
            table = header.group(1).strip()
            yield line, table, None
            continue
        yield line, table, TOML_VERSION_RE.match(line)


def _extract_json(filepath: Path) -> list[VersionEntry]:
    try:
        data = json.loads(filepath.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, UnicodeDecodeError):
        return []
    if not isinstance(data, dict):
        return []
    results = []
    name = data.get("name")
    if "version" in data and "plugins" not in data:
        results.append(VersionEntry(filepath, "version", data["version"], "json", plugin_name=name))
    for plugin in data.get("plugins", []):
        if isinstance(plugin, dict) and "version" in plugin:
            pname = plugin.get("name", "?")
            results.append(VersionEntry(
                filepath, f"plugins[{pname}].version", plugin["version"], "json", plugin_name=pname
            ))
    return results


def _extract_toml(filepath: Path) -> list[VersionEntry]:
    """只在权威 table（project / tool.poetry / package）或顶层取版本，避免误匹配。"""
    text = filepath.read_text(encoding="utf-8")
    results = []
    for _, table, m in _iter_toml_lines(text):
        if m and (table is None or table in TOML_VERSION_TABLES):
            results.append(VersionEntry(filepath, "version", m.group(1), "toml", section=table))
    return results


def _extract_plain(filepath: Path) -> list[VersionEntry]:
    """纯文本 VERSION 文件：取首个非空行，仅当是合法 semver 才视为版本。"""
    try:
        text = filepath.read_text(encoding="utf-8")
    except (UnicodeDecodeError, OSError):
        return []
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        if parse_version(stripped):
            return [VersionEntry(filepath, "version", stripped, "plain")]
        return []
    return []


def extract_versions(filepath: Path) -> list[VersionEntry]:
    kind = VERSION_EXTRACTORS.get(filepath.suffix)
    if kind == "json":
        return _extract_json(filepath)
    if kind == "toml":
        return _extract_toml(filepath)
    if filepath.name == "VERSION":
        return _extract_plain(filepath)
    return []


# ── 写入 ─────────────────────────────────────────────────────

def apply_bump_json(filepath: Path, entry: VersionEntry, new_version: str):
    """结构化 JSON 编辑，精确更新目标版本字段。"""
    data = json.loads(filepath.read_text(encoding="utf-8"))

    if entry.label == "version":
        data["version"] = new_version
    elif entry.label.startswith("plugins["):
        for plugin in data.get("plugins", []):
            if plugin.get("name") == entry.plugin_name:
                plugin["version"] = new_version
                break

    filepath.write_text(
        json.dumps(data, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )


def apply_bump_toml(filepath: Path, entry: VersionEntry, new_version: str):
    """只替换目标 section 内的第一处 version，保留引号风格与原有格式。"""
    text = filepath.read_text(encoding="utf-8")
    out = []
    done = False
    for line, table, m in _iter_toml_lines(text):
        if not done and m and table == entry.section:
            line = line[: m.start(1)] + new_version + line[m.end(1):]
            done = True
        out.append(line)
    filepath.write_text("".join(out), encoding="utf-8")


def apply_bump_plain(filepath: Path, old: str, new: str):
    """纯文本 VERSION：替换首个等于 old 的行，保留尾随换行。"""
    text = filepath.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)
    for i, line in enumerate(lines):
        if line.strip() == old:
            lines[i] = line.replace(old, new, 1)
            break
    filepath.write_text("".join(lines), encoding="utf-8")


# ── 命令 ─────────────────────────────────────────────────────

def cmd_list(root: Path, patterns: list[str], plugin_filter: str | None):
    files = find_files(root, patterns)
    if not files:
        sys.exit("未找到版本文件")

    found = False
    for f in files:
        for entry in extract_versions(f):
            if not entry.matches(plugin_filter):
                continue
            parsed = parse_version(entry.version)
            valid = "ok" if parsed else "invalid"
            rel = f.relative_to(root)
            print(f"  {rel}  {entry.label} = {entry.version}  ({valid})")
            found = True

    if not found:
        if plugin_filter:
            sys.exit(f"未找到插件: {plugin_filter}")
        sys.exit("未找到版本号")


def cmd_bump(root: Path, bump_type: str, patterns: list[str], dry_run: bool, plugin_filter: str | None):
    files = find_files(root, patterns)
    if not files:
        sys.exit("未找到版本文件")

    entries = []
    for f in files:
        for entry in extract_versions(f):
            if entry.matches(plugin_filter) and parse_version(entry.version):
                entries.append(entry)

    if not entries:
        if plugin_filter:
            sys.exit(f"未找到插件: {plugin_filter}")
        sys.exit("未找到有效的 semver 版本号")

    by_version: dict[str, list[VersionEntry]] = {}
    for entry in entries:
        by_version.setdefault(entry.version, []).append(entry)

    mode = "[DRY-RUN] " if dry_run else ""
    count = 0
    modified_files = set()

    for old_version in sorted(by_version, key=lambda v: parse_version(v)):
        parsed = parse_version(old_version)
        new_version = bump_version(*parsed, bump_type)
        print(f"{mode}{bump_type}: {old_version} -> {new_version}")

        for entry in by_version[old_version]:
            rel = entry.filepath.relative_to(root)
            print(f"  {rel}  ({entry.label})")
            if not dry_run:
                if entry.kind == "toml":
                    apply_bump_toml(entry.filepath, entry, new_version)
                elif entry.kind == "plain":
                    apply_bump_plain(entry.filepath, old_version, new_version)
                else:
                    if entry.filepath.resolve() in modified_files:
                        entry.version = new_version
                    apply_bump_json(entry.filepath, entry, new_version)
                    modified_files.add(entry.filepath.resolve())
            count += 1
        print()

    print(f"{'预览' if dry_run else '更新'}了 {count} 处版本号")


def main():
    parser = argparse.ArgumentParser(
        description="通用语义化版本 bump 工具",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "示例:\n"
            "  %(prog)s list                          列出所有版本号\n"
            "  %(prog)s list --plugin mineru           列出 mineru 版本\n"
            "  %(prog)s patch                          全部 patch\n"
            "  %(prog)s patch --plugin mineru          只 bump mineru\n"
            "  %(prog)s minor --dry-run                预览次版本号 +1\n"
            "  %(prog)s patch --file package.json      只更新指定文件\n"
        ),
    )
    parser.add_argument(
        "type",
        choices=["major", "minor", "patch", "list"],
        help="bump 类型，或 list 列出版本",
    )
    parser.add_argument("--plugin", help="只 bump 指定插件（按 name 字段匹配）")
    parser.add_argument("--dry-run", action="store_true", help="仅预览，不写入")
    parser.add_argument("--root", default=".", help="项目根目录（默认当前目录）")
    parser.add_argument("--file", nargs="+", help="指定版本文件（跳过自动扫描）")
    parser.add_argument("--include", nargs="+", help="追加 glob 扫描模式")
    args = parser.parse_args()

    root = Path(args.root).resolve()
    if not root.is_dir():
        sys.exit(f"目录不存在: {root}")

    if args.file:
        patterns = args.file
    else:
        patterns = list(DEFAULT_PATTERNS)
        if args.include:
            patterns.extend(args.include)

    if args.type == "list":
        cmd_list(root, patterns, args.plugin)
    else:
        cmd_bump(root, args.type, patterns, args.dry_run, args.plugin)


if __name__ == "__main__":
    main()
