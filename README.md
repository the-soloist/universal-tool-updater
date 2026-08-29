# Universal Tool Updater

一个使用 Rust 编写的跨平台工具更新器。通过 YAML 描述工具的版本来源、下载产物和安装方式，Updater 负责并行完成版本解析、断点续传、解压处理、事务安装与状态记录。

适用于维护本地工具箱，尤其是来源分散、目录结构不统一、需要批量更新的便携工具集合。

## 核心能力

- **多种版本来源**：支持 GitHub Release、网页正则匹配和 HTTP 文件元数据。
- **灵活的产物选择**：支持 GitHub Asset、GitHub Source、页面链接、固定 URL 和版本 URL 模板。
- **完整任务并行**：每个工具的解析、下载、解压、处理和安装作为一个独立任务并行执行。
- **可靠下载**：支持 HTTP 断点续传、远端文件重新校验、瞬时错误重试和多产物隔离。
- **事务安装**：在 staging 中完成准备，提交失败时恢复原有安装，避免留下半成品。
- **跨文件系统提交**：staging 与目标不在同一文件系统时，自动复制到目标附近后再原子切换。
- **自适应终端界面**：并发进度条和目录树会根据终端宽度调整字段、文件名与进度条长度。
- **严格配置检查**：拒绝未知字段、非法路径、无效正则、重复 ID 以及互相冲突的参数组合。
- **安全归档处理**：支持 ZIP、7z、RAR、tar.gz、tar.bz2、tar.xz、gz 和 xz，并阻止解压路径逃逸。
- **跨平台扩展**：常见文件操作由 Rust 原生 action 完成，复杂逻辑可使用受约束的 Python 3 hook。
- **安全自更新**：从 GitHub Release 下载当前平台构建，验证 SHA-256 和程序版本后再替换自身。

## 快速开始

### 1. 获取 Updater

从 [GitHub Releases](https://github.com/the-soloist/universal-tool-updater/releases/latest) 下载对应平台的压缩包，或从源码构建：

```bash
./build.sh
```

默认构建产物为：

```text
target/release/updater       # Linux / macOS
target/release/updater.exe   # Windows
```

### 2. 创建 profiles

Updater 默认读取当前工作目录下的 `profiles/manifest.yaml`。一个最小配置由 manifest 和至少一个 profile 文件组成：

```text
profiles/
├── manifest.yaml
└── tools.yaml
```

`profiles/manifest.yaml`：

```yaml
schema_version: 5

include:
  - tools.yaml

paths:
  toolkit_root: ~/Tools/Toolkit
  downloads: updates
  staging: updates/staging
  state: .updater/state.yaml

network:
  jobs: 4

defaults:
  create_destination: true
  install:
    input: extract
    existing: replace
    save: directory
    strip_single_root: true
    archive_name: '{name} - {version}.7z'
```

`profiles/tools.yaml`：

```yaml
tools:
  ripgrep:
    name: ripgrep
    release:
      type: github
      repository: BurntSushi/ripgrep
    artifacts:
      - type: github-asset
        pattern: '^ripgrep-[^-]+-x86_64-pc-windows-msvc\.zip$'
    install:
      destination: Tools/ripgrep
```

Profile 名称来自文件名，因此 `tools.yaml` 对应 `tools` profile。只有 `manifest.yaml` 的 `include` 中列出的文件会被加载。

可直接复制修改的完整配置见 [examples/manifest.yaml](examples/manifest.yaml) 和 [examples/profile.yaml](examples/profile.yaml)；全部字段、组合约束和更多说明见 [YAML 配置文件编写规范](docs/YAML_CONFIGURATION.md)。

### 3. 检查并运行

```bash
# 检查全部 YAML 和跨字段约束；不会访问网络
updater check

# 查看工具列表
updater list

# 以 profile > 目录 > 子目录 > 工具的层级表格展示
updater list --tree

# 预览指定工具的版本和下载地址
updater update ripgrep --dry-run

# 更新指定工具
updater update ripgrep

# 更新全部工具
updater update
```

不指定子命令时等同于 `updater update`，会更新当前筛选范围内的全部工具。

## 命令

| 命令 | 用途 |
| --- | --- |
| `updater check` | 离线检查 manifest、所有 include 文件及参数冲突。 |
| `updater list` | 按 profile、工具名称列出配置中的工具。 |
| `updater list --tree` | 以自适应宽度的层级表格展示工具分布。 |
| `updater update [TOOLS]...` | 更新全部工具或指定工具 ID。 |
| `updater migrate --input DIR --output DIR` | 将旧版 TOML 配置转换为当前 YAML schema。 |
| `updater self-update` | 下载、验证并安装最新 Updater。 |

常用全局参数：

```bash
# 指定包含 manifest.yaml 的 profiles 目录
updater --profiles /path/to/profiles check

# 直接指定 manifest；不能与 --profiles 同时使用
updater --manifest /path/to/manifest.yaml check

# 只处理一个或多个 profile
updater --profile tools --profile web update

# 输出诊断日志，同时隐藏动态进度界面
updater --verbose update ripgrep

# 修改日志目录
updater --log-dir /path/to/logs update
```

`--profile` 也可以写为 `-p`、`--group` 或 `-g`。

常用更新参数：

```bash
# 忽略已记录的版本并重新安装
updater update ripgrep --force

# 覆盖 manifest 中的 network.jobs
updater update --jobs 8

# 允许创建被工具配置禁止自动创建的目标目录
updater update ripgrep --create-missing

# 禁用动态进度显示
updater update --no-progress
```

使用 `updater <COMMAND> --help` 可查看完整参数。

## 配置与路径

路径采用两个不同的相对基准：

| 配置 | 相对路径基准 | 默认值 |
| --- | --- | --- |
| `paths.toolkit_root` | Updater 启动时的工作目录 | 必填 |
| `paths.downloads` | Updater 可执行文件所在目录 | `updates` |
| `paths.staging` | Updater 可执行文件所在目录 | `<downloads>/staging` |
| `paths.state` | `toolkit_root` | `.updater/state.yaml` |
| `install.destination` | `toolkit_root` | 必填 |

推荐将 `downloads` 和 `staging` 放在 Updater 所在的本地目录，将最终工具目录放在 `toolkit_root`。这样可以减少解压、合并和压缩过程对同步盘的影响。

每次运行会创建隔离的 `run-*` 工作目录：

```text
updates/
├── .partial/          # 未完成分片及已下载、等待安装的产物
└── staging/
    └── run-*/         # 本次更新的事务暂存，结束后清理
```

多产物工具会保留每个已完成的下载，直到该工具整体安装成功。运行被中断后，下一次更新会校验并复用这些文件，从第一个尚未完成的产物继续；旧版本遗留在 `run-*/<tool-id>/downloads/` 中的完整文件也会自动恢复，无需手工移动。

每个 `.part` 文件都有一份 schema v2 YAML 元数据，记录 URL、文件名、远端总大小、ETag / Last-Modified、已确认字节数、SHA-256、完成状态和校验级别。下载过程中每 8 MiB 先同步文件，再原子更新 SHA-256 校验点；下次续传前会重新计算已有分片的哈希。哈希不一致的缓存会被删除并从头下载，异常退出后位于最后一个校验点之后的未确认尾部会被截断。`verified: transport` 表示 HTTP 传输完整性已经确认，不代表压缩包内容已经通过解压校验。

如果 staging 和安装目标位于同一文件系统，最终通过 `rename` 原子提交；如果跨文件系统，则只在最终提交阶段于目标父目录创建短生命周期的 `.工具名-commit-*` 目录。

## 更新与安装语义

一个工具的完整更新流程如下：

```text
解析版本 → 选择产物 → 下载/续传 → 解压或复制 → 执行 hook → 生成输出 → 事务提交 → 记录版本
```

- `install.input: extract`：解压下载产物后安装。
- `install.input: copy`：保留下载产物，不执行解压。
- `install.existing: replace`：使用新内容替换原目录。
- `install.existing: merge`：把已有内容合并进新安装内容。
- `install.save: directory`：以目录形式保存，并在 `<destination>/.version` 记录版本。
- `install.save: archive`：完成处理后重新压缩为 7z。

GitHub Asset、GitHub Assets 和 GitHub Source 在 `input: copy` 时不会被二次压缩；原始文件保存在 `<destination>/release`，版本记录保存在 `<destination>/.version`。

工具成功安装后还会更新 `paths.state`。状态文件采用临时文件加原子替换写入，安装或状态记录失败会在结果摘要中明确显示。

## 并发、进度与日志

`network.jobs` 控制同时运行的完整工具更新任务数，`update --jobs N` 可在单次运行中覆盖它。下载、解压和压缩不会拆分为互相独立的全局阶段，因此慢速压缩不会阻塞其他工具开始下载或安装。

交互式终端使用统一的多进度区域，每一行显示：

```text
profile › tool  filename (index/total)  progress  size  eta
```

字段宽度会随终端变化；窄终端会依次缩短文件名、收缩进度条并隐藏次要字段，避免多任务输出互相覆盖。使用 `--verbose` 或 `--no-progress` 时输出稳定的普通日志，更适合重定向和 CI。

每次运行都会在 Updater 可执行文件旁的 `logs/` 中创建独立日志，记录解析结果、下载 URL、文件名、字节数、目标路径和完整错误链。可使用 `--log-dir` 覆盖位置。

## GitHub Token

访问 GitHub API 时建议通过环境变量提供 Token，不要写入 YAML：

```bash
export GITHUB_TOKEN=github_pat_xxx
```

环境变量名称由 `network.github_token_env` 配置，默认是 `GITHUB_TOKEN`。

## Hook

简单文件操作应使用原生 action：

```yaml
hooks:
  after_unpack:
    - type: rename
      from: 'tool-*.jar'
      to: tool.jar
    - type: move-contents
      from: release
      to: .
```

无法用原生 action 表达时可以调用 Python 3：

```yaml
hooks:
  after_unpack:
    - type: python
      script: ./scripts/normalize.py
      args: [--strict]
      timeout_seconds: 60
      working_directory: staging
```

可通过 `UTU_PYTHON` 指定解释器。Updater 不执行任意 Shell、Batch 或 PowerShell hook；路径和工作目录会在运行前校验。

## 自更新

```bash
# 只检查最新版本
updater self-update --check

# 安装最新版本
updater self-update

# 强制重新安装当前版本
updater self-update --force

# Windows：查询异步替换结果
updater self-update --status
```

自更新会下载平台压缩包和 `SHA256SUMS.txt`，校验 SHA-256，并运行候选程序的 `--version` 后才执行替换。Linux 和 macOS 会直接原子替换；Windows 会在当前进程退出后由临时 helper 完成替换，调度成功时退出码为 `10`，最终结果可通过 `--status` 查询。

目前发布目标：

- Linux x86_64 musl
- Windows x86_64 MSVC
- macOS Universal（Apple Silicon + Intel）

## 构建

项目要求 Rust `1.93` 或更高版本，并使用 `Cargo.lock` 固定依赖：

```bash
# 当前平台
./build.sh

# 指定 Rust target
./build.sh --target x86_64-unknown-linux-musl
./build.sh --target x86_64-pc-windows-gnu

# macOS Universal
rustup target add aarch64-apple-darwin x86_64-apple-darwin
./build.sh --macos-universal
```

Linux 和 Windows GNU 交叉构建需要目标链接器；macOS 上安装 Zig、`cargo-zigbuild` 并执行 `rustup target add x86_64-pc-windows-gnu` 后，脚本会自动使用 Zig 构建 Windows GNU 版本。Windows GNU 产物位于 `target/x86_64-pc-windows-gnu/release/updater.exe`。macOS Universal 产物位于：

```text
target/universal-apple-darwin/release/updater
```

## 发布

Release workflow 只在推送 `v*` tag 时运行，且 tag 必须与 `Cargo.toml` 版本一致：

```bash
git tag v0.2.0
git push origin v0.2.0
```

GitHub Actions 会构建三个平台的 7z 包、生成 `SHA256SUMS.txt` 并创建 GitHub Release。

## 开发验证

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```
