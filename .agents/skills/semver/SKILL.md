---
name: semver
description: |
  管理项目的语义化版本号，判断 major、minor 或 patch，并统一更新 VERSION、pyproject.toml、Cargo.toml、package.json 和 Claude 插件清单。
  当用户要求更新或升级版本号、bump version、发布 release、选择 patch/minor/major，或询问当前变更应该递增哪一级版本时使用。
---

# 语义化版本管理

按 [Semantic Versioning 2.0.0](https://semver.org/lang/zh-CN/) 判断版本级别，并使用插件自带脚本统一更新项目中的静态版本号。

脚本位于本 `SKILL.md` 同级的 `scripts/bump.py`，仅依赖 Python 标准库。执行命令前，将下文的 `<skill-dir>` 替换为本 `SKILL.md` 所在目录的绝对路径。优先使用可用的 `python3` 或 `python`；只有环境未提供 Python 命令时才使用 `uv run python`。

## 判断版本级别

| 类型 | 适用变更 | 示例 |
|------|----------|------|
| **MAJOR** | 不兼容的 API 或行为变更 | 删除公共接口、改变既有行为语义 |
| **MINOR** | 向下兼容的新功能 | 新插件、新参数、新能力 |
| **PATCH** | 向下兼容的问题修正 | bug、文档、编码或路径修正 |

- MINOR 递增时 PATCH 归零；MAJOR 递增时 MINOR 和 PATCH 归零。
- `0.y.z` 表示初始开发阶段，但仍按变更影响选择递增级别。
- 已发布版本不可原地修改；修正必须产生新版本。
- 脚本只处理 `x.y.z` 静态版本。预发布版本和动态版本方案需要按项目自身工具处理。

用户已明确指定 `major`、`minor` 或 `patch` 时采用该类型。用户要求更新版本但没有指定类型时，检查工作区变更与近期提交，判断类型并先说明依据。用户仅询问应该使用哪个版本时，只给出判断，不修改文件。

## 执行工作流

### 查看版本

用户要求查看或列出版本时运行：

```bash
python3 "<skill-dir>/scripts/bump.py" list
```

可用 `--plugin <name>`、`--file <path>`、`--include <glob>` 或 `--root <dir>` 缩小范围。

### 更新版本

在用户要求实际更新版本号时，必须先 dry-run：

```bash
python3 "<skill-dir>/scripts/bump.py" <major|minor|patch> --dry-run
```

把用户提供的 `--plugin`、`--file`、`--include` 或 `--root` 参数原样加到预览命令。检查预览范围和新版本正确后，再去掉 `--dry-run` 执行正式更新：

```bash
python3 "<skill-dir>/scripts/bump.py" <major|minor|patch>
```

完成后报告旧版本、新版本和实际修改的文件。不要因为版本更新自动扩大到 commit、tag 或 push。

## 默认扫描范围

脚本递归扫描并跳过 `node_modules`、`.venv`、`dist`、`build`、`__pycache__` 等依赖或构建目录。

| 文件 | 版本字段 |
|------|----------|
| `VERSION` | 首个非空行 |
| `pyproject.toml` | `[project].version` 或 `[tool.poetry].version` |
| `Cargo.toml` | `[package].version` |
| `package.json` | 顶层 `version` |
| `**/.claude-plugin/plugin.json` | 顶层 `version` |
| `.claude-plugin/marketplace.json` | `plugins[].version` |

TOML 单引号和双引号都会保留，且只修改权威 table 中的 `version`。`dynamic = ["version"]`、setuptools_scm 等动态版本不会被脚本更新。

## 发布与 Git 标签

只有用户明确要求发布、提交、打 tag 或推送时才执行相应 Git 操作。顺序必须是：

1. 更新并核验版本文件。
2. commit 版本变更。
3. 在该 commit 上创建 tag。
4. 获得外部写入授权后推送 commit 和 tag。

多插件独立版本使用带插件名的 tag，例如 `mineru-v0.2.5`；单一版本项目使用 `v0.2.5`。禁止先 tag 再 commit，否则 tag 会指向不含版本变更的旧提交。
