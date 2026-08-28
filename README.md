# Universal Tool Updater

使用 Rust 编写的跨平台、配置驱动工具更新器。它负责发现新版本、解析下载产物、下载和安全解压，并通过事务安装将工具更新到本地工具箱。

默认工具箱目录为：

```text
~/Tools/Toolkit
```

状态文件和所有相对安装路径都位于该目录下；下载目录默认是 `updater` 可执行文件旁的 `updates/`，安装事务暂存默认位于 `updates/staging/`。全局设置位于 [`profiles/manifest.yaml`](profiles/manifest.yaml)。

## 特性

- 支持 GitHub Release、网页正则、URL 模板和 HTTP Header 版本检测。
- 强类型 YAML schema，启动前检查未知字段、无效正则、重复 ID、路径逃逸和来源/产物组合。
- 支持 ZIP、7z、RAR、tar.gz、tar.bz2、tar.xz、gz 和 xz；RAR4/RAR5 由纯 Rust 后端解压，不依赖外部程序。
- 每个工具使用独立配置对象，来源解析、下载、归档、安装、钩子和状态管理互不依赖。
- 解压后的重命名和目录整理由 Rust 原生 action 完成，不依赖 Batch、PowerShell 或 Unix Shell。
- 复杂扩展只允许 Python 3 脚本；解释器可跨平台自动发现，也可通过 `UTU_PYTHON` 指定。
- 下载支持 HTTP 断点续传；未完成分片持久保留，安装在独立 staging 中完成耗时处理，并通过原子提交和备份在失败时恢复旧版本。
- 以工具的完整更新流程为并发任务；每个工具使用独立临时目录，下载、解压、压缩和安装不会互相覆盖。
- 并发进度使用统一终端区域显示，并根据终端宽度切换完整、紧凑或极简布局。
- 配置文件只描述工具，运行状态原子写入 `~/Tools/Toolkit/.updater/`。
- 同一份 profile 配置可由 Windows、Linux 和 macOS 版本的 updater 加载。

## 构建

需要稳定版 Rust 工具链：

```bash
./build.sh
```

产物位于：

```text
target/release/updater
target/release/updater.exe
```

## 跨平台构建

Rust 支持指定目标三元组构建。本项目使用条件编译隔离 Windows/Unix 差异，并在 CI 中分别使用 Windows、Linux 和 macOS 原生 runner 构建。

```bash
# 当前平台
./build.sh

# macOS Universal（同时支持 Apple Silicon 与 Intel）
rustup target add aarch64-apple-darwin x86_64-apple-darwin
./build.sh --macos-universal

# 安装目标后指定构建目标；交叉构建还需要目标平台链接器
rustup target add x86_64-unknown-linux-musl
./build.sh --target x86_64-unknown-linux-musl
```

release profile 会启用 thin LTO、单 codegen unit、`panic=abort`，并在链接阶段移除调试信息和符号表。脚本使用 `Cargo.lock` 构建并输出产物路径、体积和文件类型。

macOS Universal 产物位于 `target/universal-apple-darwin/release/updater`。Linux 发布目标使用 `x86_64-unknown-linux-musl`，构建机需提供 `musl-gcc`（Ubuntu 安装 `musl-tools`）；macOS 上若已安装 `cargo-zigbuild` 和 Zig，脚本会自动使用它们。Windows 使用 `x86_64-pc-windows-msvc`。其他交叉链接场景建议使用 `cargo-xwin` 或交给 CI 的目标系统原生构建。

## 发布 Release

推送与 `Cargo.toml` 版本一致的 `v*` 标签后，GitHub Actions 会自动构建 Linux x86_64 musl、Windows x86_64 和 macOS Universal 版本，并创建 GitHub Release、上传压缩包及 `SHA256SUMS`：

- `updater-vX.Y.Z-linux.tar.gz`
- `updater-vX.Y.Z-windows.zip`
- `updater-vX.Y.Z-macos.tar.gz`

```bash
git tag v0.1.0
git push origin v0.1.0
```

如果标签版本与 `Cargo.toml` 不一致，Release workflow 会停止发布。

## 使用

程序默认加载当前目录下的 `profiles/manifest.yaml`：

```bash
# 校验全部配置，不访问网络
updater check

# 列出工具及最终安装目录
updater list

# 以合并单元格的终端表格展示 profile、目录层级和工具（自动适配终端宽度）
updater list --tree

# 只展示指定 profile 的分布
updater list --tree --profile web

# 更新全部工具
updater update

# 覆盖 manifest.yaml 中的 network.jobs
updater update --jobs 4

# 更新指定工具
updater update bat ripgrep

# 只处理指定 profile
updater update --profile tools

# 仅解析版本和下载地址
updater update bat --dry-run

# 忽略本地状态
updater update bat --force
```

`check` 会加载 `manifest.yaml` 及全部 include profile，但不会访问网络或修改工具目录。它会检查 YAML 语法、必填字段、未知字段、枚举值、网络参数、HTTP(S) URL、HTTP 头、正则表达式、模板占位符、路径安全性和 Hook 参数，同时检查 release 与 artifact 类型、安装参数、工具目录、状态文件、下载目录、事务暂存目录及符号链接之间的冲突。失败时会返回非零退出码，并指出对应配置文件、工具和原因。

`--profiles` 直接指向包含 `manifest.yaml` 的目录。所有平台默认使用项目根目录下的 `profiles/`；需要切换配置时可显式指定其他目录：

```bash
updater --profiles /Users/admin/Projects/Github/universal-tool-updater/profiles check
```

`--profiles` 与直接指定单个文件的 `--manifest` 互斥：

```bash
updater --profiles /path/to/profiles check
updater --manifest /path/to/manifest.yaml check
```

如需调用 GitHub API，将 Token 放入环境变量，不要写入 YAML：

```bash
export GITHUB_TOKEN=github_pat_xxx
```

每次运行都会在 `updater` 可执行文件旁的 `logs/` 中保留独立日志。日志包含工具处理结果、下载 URL、文件名、字节数、保存路径和完整错误信息。可用全局参数 `--log-dir` 覆盖日志目录：

```bash
updater --profiles ./profiles --log-dir ~/Logs/universal-tool-updater update frida
```

## 配置结构

完整字段、约束、类型兼容关系和示例见 [YAML 配置文件编写规范](docs/YAML_CONFIGURATION.md)。

profiles 目录中的 `manifest.yaml` 负责公共路径、网络参数、默认安装策略和 include 列表。只有 include 中声明的 YAML 会被加载；每个 include 文件就是一个 profile，profile 名称取文件名去掉 `.yaml` 后的部分。例如 `web.yaml` 中的工具属于 `web` profile：

```yaml
schema_version: 5
include:
  - develop.yaml
  - tools.yaml

paths:
  toolkit_root: ~/Tools/Toolkit
  downloads: updates
  staging: updates/staging
  state: .updater/windows-state.yaml

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

相对 `downloads` 和 `staging` 以 `updater` 可执行文件所在目录为基准；相对 `state` 和工具安装目标仍以 `toolkit_root` 为基准。绝对路径不做重新拼接。省略 `staging` 时默认使用 `<downloads>/staging`。
`network.jobs` 必须大于 0，表示默认同时执行的完整工具下载/更新任务数；命令行 `update --jobs N` 的优先级更高。
每次运行会分别在 `downloads` 和 `staging` 下创建独立的 `run-*` 临时目录，并进一步按工具 ID 隔离；运行结束后自动清理。下载、解压位于 `downloads`，合并已有内容、生成版本文件和压缩 7z 位于 `staging`。未完成的下载保存在 `downloads/.partial/<tool-id>/`，下次更新使用 HTTP `Range` 和可用的 `If-Range` 校验器继续下载；服务端不支持 Range 或远端文件已变化时会自动从头下载。工具成功安装后会清理对应分片。

最终提交时，如果 `staging` 和工具目标位于同一文件系统，updater 会直接通过 `rename` 原子切换；如果位于不同文件系统，会先复制到目标父目录中的短生命周期 `.工具名-commit-*` 目录，再执行原子切换。因此把 `staging` 放到 `updates` 可以避免解压、合并和压缩期间在同步盘产生长期瞬态目录，跨盘时仅最终传输阶段会短暂出现相邻目录。

每个 profile 文件的 `tools` 是字典，每个工具 ID 对应一个独立字典节点。ID 必须使用小写 kebab-case，多单词名称以 `-` 拼接，例如 `context-menu-manager`，不允许大写字母、空格、下划线或连续的 `-`。工具条目由四个明确部分组成：版本来源、下载产物、安装策略和可选钩子。

```yaml
tools:
  context-menu-manager:
    name: ContextMenuManager
    release:
      type: github
      repository: BluePointLilac/ContextMenuManager
    artifacts:
      - type: github-asset
        pattern: ContextMenuManager.zip
    install:
      destination: Tools/ContextMenuManager
```

`github-asset` 只选择第一个匹配项；需要下载同一 release 中所有匹配产物时使用
`github-assets`。例如 Frida 可以用一个规则自动覆盖当前与后续新增的平台：

```yaml
artifacts:
  - type: github-assets
    pattern: ^frida-server-.+\.xz$
```

所有相对 `destination` 和符号链接目标均以 `toolkit_root` 为根目录，禁止使用 `..` 逃逸。

### 全局保存形式

保存形式统一在平台的 `manifest.yaml` 中配置。`directory` 表示保存解压后的目录；`archive` 表示完成解压、原生 action 和合并后，再由 Rust 原生 7z 后端压缩保存：

```yaml
defaults:
  install:
    save: archive
    archive_name: '{name}#{version}.7z'
```

使用 `archive` 时，每个工具的 `destination` 目录只保存生成的压缩包。`archive_name` 支持 `{id}`、`{name}`、`{version}` 占位符，目前压缩格式固定为 7z。

使用 `directory` 时，updater 会在 `<destination>/.version` 写入当前版本，并与目录内容一起原子提交。`input: copy` 的目标会自动使用 `<destination>/release` 保存原始文件，因此版本标记保存在配置目录的 `<destination>/.version`，而不是 `release/.version`。

`input: copy` 的 GitHub Asset、GitHub Assets 和 GitHub Source 属于原始发布资源：updater 会将下载文件直接保存到 `destination/release`，不再二次压缩为 7z，并在 `destination/.version` 中记录当前版本。使用 `input: extract` 的工具不受此规则影响。

### Hook actions

Hook 阶段是有序 action 列表。简单文件操作必须使用 Rust 原生 action；`rename` 会在当前产物的解压目录中递归查找，并要求文件名通配符恰好匹配一个文件：

```yaml
hooks:
  after_unpack:
    - type: rename
      from: JDumpSpider*.jar
      to: JDumpSpider.jar
```

`move-contents` 用于提升子目录内容，来源和目标都相对于当前解压目录：

```yaml
hooks:
  after_unpack:
    - type: move-contents
      from: release
      to: .
```

只有无法通过原生 action 表达的复杂逻辑才使用 Python 3。项目不执行 Shell 命令，也不接受 Batch、PowerShell 或任意可执行文件作为 Hook：

```yaml
hooks:
  after_unpack:
    - type: python
      script: ./scripts/custom/normalize.py
      args:
        - --strict
      timeout_seconds: 60
      working_directory: staging
      environment:
        MODE: portable
```

Windows 依次查找 `py -3`、`python3` 和 `python`，Linux/macOS 依次查找 `python3` 和 `python`，并验证解释器确实是 Python 3；设置 `UTU_PYTHON` 可指定解释器。Python action 默认超时 300 秒，并会收到 `UTU_TOOL_ID`、`UTU_TOOL_NAME`、`UTU_VERSION`、`UTU_TOOLKIT_ROOT`、`UTU_DOWNLOAD_DIR`、`UTU_STAGING_DIR` 和 `UTU_INSTALL_DIR`。配置不能覆盖这些保留变量。原生 action 会限制在解压目录内，并拒绝路径逃逸、覆盖目标和不确定的多文件匹配。

## 旧配置迁移

仍可使用独立迁移命令把旧版平铺 TOML 转换为当前 YAML schema；运行时只加载 YAML。旧 Hook 只有 `.py` 脚本可以自动迁移，Batch、PowerShell 和其他命令必须改写为原生 action：

```bash
updater migrate --input ./legacy/windows --output ./profiles
```

迁移完成后应执行：

```bash
updater --profiles ./profiles check
```

## 开发验证

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```
