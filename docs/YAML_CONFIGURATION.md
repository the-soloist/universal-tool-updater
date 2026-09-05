# YAML 配置文件编写规范

本文档描述 Universal Tool Updater 当前使用的 YAML schema。配置由一个 `manifest.yaml` 和若干 profile 文件组成；当前 schema 版本为 `5`。

解析器会拒绝未知字段、错误枚举值和不满足跨字段约束的配置。修改配置后必须执行 `updater check`，不要只依赖 YAML 编辑器的语法检查。

## 1. 推荐目录结构

```text
profiles/
├── manifest.yaml
├── crypto.yaml
├── develop.yaml
├── misc.yaml
├── mobile.yaml
├── pwn.yaml
├── reverse.yaml
├── tools.yaml
└── web.yaml
```

- `manifest.yaml` 保存全局路径、网络参数、安装默认值和 profile 清单。
- 其他 `.yaml` 文件保存工具字典。文件名去掉 `.yaml` 后就是 profile 名称，例如 `reverse.yaml` 对应 `reverse`。
- 只有 `manifest.yaml` 的 `include` 中明确列出的文件会被加载。
- include 必须是位于 manifest 目录内的安全相对路径，扩展名必须为 `.yaml`，不能包含 `..`，也不能包含 `manifest.yaml` 本身。
- 不同 include 文件不能产生相同的 profile 名称；工具 ID 在所有 profile 中必须全局唯一。

配置文件统一使用 UTF-8。建议使用两个空格缩进，不使用 Tab。正则表达式、路径模板以及包含 `#`、`:`、`{}` 的字符串建议使用单引号，避免被 YAML 解释为注释或特殊语法。

## 2. manifest.yaml

完整的 manifest 示例：

```yaml
schema_version: 5

include:
  - crypto.yaml
  - develop.yaml
  - misc.yaml
  - mobile.yaml
  - pwn.yaml
  - reverse.yaml
  - tools.yaml
  - web.yaml

paths:
  toolkit_root: ~/Tools/Toolkit
  downloads: updates
  staging: updates/staging
  state: .updater/state.yaml

network:
  user_agent: Universal-Tool-Updater/3
  timeout_seconds: 60
  progress: true
  github_token_source: env:GITHUB_TOKEN
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

### 2.1 顶层字段

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `schema_version` | 是 | 必须为 `5`。 |
| `include` | 是 | 非空的 profile 文件列表。 |
| `paths` | 是 | 全局路径配置。 |
| `network` | 否 | 网络和并发配置；省略时使用默认值。 |
| `defaults` | 否 | 工具安装默认值；工具自身的同名字段优先。 |
| `extraction_limits` | 否 | 解压与下载配额；省略时使用默认值（见 2.5）。 |

### 2.2 paths

| 字段 | 必填 | 默认值 | 相对路径基准 |
| --- | --- | --- | --- |
| `toolkit_root` | 是 | 无 | updater 启动时的工作目录 |
| `downloads` | 否 | `updates` | updater 可执行文件所在目录 |
| `staging` | 否 | `<downloads>/staging` | updater 可执行文件所在目录 |
| `state` | 否 | `.updater/state.yaml` | `toolkit_root` |

所有路径都不能为空。绝对路径直接使用；`~` 和 `~/...` 会展开到当前用户主目录。

`downloads` 用于本次下载文件、断点续传分片及等待工具整体安装成功的完整产物；工具安装成功后会立即删除该工具对应的完整下载文件和 partial 缓存，安装失败或运行中断时则保留，以便下次运行校验并复用。断点缓存的 schema v2 元数据包含已确认字节数和 SHA-256；续传前会重新计算分片哈希，损坏缓存会被丢弃。`verified: transport` 仅表示 HTTP 传输完整。`staging` 用于合并、压缩和安装事务。`staging` 不能与 `state` 重叠。工具安装路径、版本文件和符号链接目标也不能与 `downloads`、`staging`、`state` 重叠。

### 2.3 network

| 字段 | 类型 | 默认值 | 约束 |
| --- | --- | --- | --- |
| `user_agent` | string | `Universal-Tool-Updater/3` | 非空且必须是合法 HTTP Header 值。 |
| `timeout_seconds` | integer | `60` | 必须大于 `0`。 |
| `progress` | boolean | `true` | 是否显示下载进度。 |
| `github_token_source` | string | `env:GITHUB_TOKEN` | GitHub token 来源：`env:变量名` 从环境变量读取，或使用 `gh auth token` 调用 GitHub CLI；不会把 token 写入 YAML。 |
| `jobs` | integer | `4` | 同时执行的完整工具更新任务数，必须大于 `0`。 |

命令行 `updater update --jobs N` 会覆盖 `network.jobs`。使用 `env:变量名` 时，GitHub token 通过对应环境变量注入；使用 `gh auth token` 时，更新器会直接调用 GitHub CLI。不要直接把 token 写入 YAML。

### 2.4 defaults

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `create_destination` | `true` | 目标不存在时是否允许创建。为 `false` 时会跳过目标不存在的工具，除非命令行使用 `--create-missing`。 |
| `install.input` | `extract` | 默认输入处理方式。 |
| `install.existing` | `replace` | 默认已有内容处理方式。 |
| `install.save` | `directory` | 默认最终保存形式。 |
| `install.strip_single_root` | `true` | 解压结果只有一个根目录时，是否提升其内容。 |
| `install.archive_name` | `{name} - {version}.7z` | `save: archive` 的输出文件名模板。 |

`archive_name` 只能是文件名，不能包含目录；支持 `{id}`、`{name}`、`{version}` 三个占位符。最终保存为压缩包时扩展名必须为 `.7z`。

### 2.5 extraction_limits

| 字段 | 类型 | 默认值 | 约束 |
| --- | --- | --- | --- |
| `max_total_bytes` | integer | `8589934592`（8 GiB） | 必须大于 `0`。 |
| `max_entries` | integer | `100000` | 必须大于 `0`。 |

解压 zip/tar/7z/rar 时逐条目累计未压缩大小和条目数量，超过任一上限即在写入前拒绝；同时统计实际写出的字节数，归档头谎报大小时也会在写入中途中断。gz/xz 单文件解压按同一 `max_total_bytes` 计数。下载侧对 `Content-Length`（或续传的完整大小）执行同一 `max_total_bytes` 检查，超限直接报错。整个节点可省略，两个子字段也均可省略（分别取默认值）。

## 3. Profile 与工具字典

每个 profile 文件只有一个顶层字段 `tools`，且字典不能为空：

```yaml
tools:
  context-menu-manager:
    name: ContextMenuManager
    enabled: true
    release:
      type: github
      repository: BluePointLilac/ContextMenuManager
    artifacts:
      - type: github-asset
        pattern: '^ContextMenuManager.*\.zip$'
    install:
      destination: Tools/ContextMenuManager
```

工具字段：

| 字段 | 必填 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `name` | 否 | 工具 ID | 展示名，也可用于压缩包名称模板。 |
| `enabled` | 否 | `true` | 为 `false` 时更新结果为 skipped。 |
| `allow_insecure_transports` | 否 | `false` | 仅允许当前工具使用 HTTP；省略时该工具只能使用 HTTPS。 |
| `release` | 是 | 无 | 版本来源，只能选择一种类型；`manual` 表示工具由用户手动维护。 |
| `artifacts` | 自动更新工具必填 | manual 工具为空 | 下载产物列表；自动更新工具不能为空，manual 工具不得配置。 |
| `install` | 是 | 无 | 安装目标和处理策略。 |
| `hooks` | 否 | 空 | 有序的原生 action 或 Python action。 |

### 3.1 工具 ID 与名称规范

工具 ID 必须使用小写 kebab-case：

```text
^[a-z0-9]+(?:-[a-z0-9]+)*$
```

正确示例：`d-beaver`、`context-menu-manager`、`think-php-bewhale`。

错误示例：`DBeaver`、`d_beaver`、`d beaver`、`d--beaver`。

ID 应优先使用工具的稳定英文名，不要使用官网域名作为 ID。存在作者区分时，ID 推荐使用 `<工具英文名>-<作者名>`，展示名推荐使用 `<工具名>@<作者名>`：

```yaml
tools:
  think-php-bewhale:
    name: ThinkPHP综合利用工具@bewhale
```

`name` 不能为空，不能有首尾空白或控制字符。由于 `{name}` 可能进入文件名，建议避免 `/`、`\\`、`:`、`*`、`?`、`"`、`<`、`>`、`|` 等跨平台不安全字符。

## 4. release：版本来源

### 4.1 GitHub Release

```yaml
release:
  type: github
  repository: fatedier/frp
  ignore_versions:
    - v0.70.0
  allow_prereleases: false
```

- `repository` 必须为合法的 `owner/repository`，不能包含 URL 或多余路径。
- `ignore_versions` 可省略，用于跳过指定 release tag；条目不能为空或重复。
- `allow_prereleases` 可省略，默认 `false`；设为 `true` 才会选择 GitHub prerelease。使用 API 时按 release 标记过滤，使用 Atom 时按 semver tag 的预发布段过滤；无法解析为 semver 的 tag 保持可用。
- 配置 GitHub token 后通过 API 解析 release；未配置 token 时使用公开 release 信息。

### 4.2 Web 页面

```yaml
release:
  type: web
  url: https://example.com/downloads
  version_pattern: 'Version\s+([0-9]+\.[0-9]+\.[0-9]+)'
  ignore_versions:
    - 1.2.0-beta
```

- `url` 必须是带主机名的 HTTPS URL；仅当当前工具设置 `allow_insecure_transports: true` 时才允许 HTTP。
- `version_pattern` 是 Rust 正则表达式，必须至少包含一个捕获组；第一个捕获组就是版本号。
- 避免使用正则 look-around 或反向引用，因为 Rust `regex` 不支持这些特性。

### 4.3 HTTP 文件元数据

```yaml
release:
  type: http
  url: https://example.com/releases/tool-latest.zip
  version_headers:
    - etag
    - last-modified
    - content-length
```

HTTP 类型会对 URL 发起 HEAD 请求，按顺序找到第一个存在的 `version_headers`，并对该 header 值计算稳定摘要作为版本号。列表不能为空，header 名称必须合法且不区分大小写地唯一。`url` 必须是带主机名的 HTTPS URL；仅当当前工具设置 `allow_insecure_transports: true` 时才允许 HTTP。

### 4.4 手动维护

```yaml
release:
  type: manual
```

`manual` 用于登记需要人工下载、授权或安装的工具。此类型不解析版本，不发起下载，也不执行安装和 hooks；执行 `update` 时结果为 `skipped`，`list` 和 `list --tree` 会显示 `[manual]` 标记。

- `artifacts` 应省略，也兼容显式空列表 `artifacts: []`；配置任何下载产物都会报错。
- `install.destination` 仍然必填，并继续参与安装目录冲突检查。
- `manual` 没有其他字段，多余字段会被拒绝。

## 5. artifacts：下载产物

同一工具可以配置多个 artifact，按配置顺序解析和安装。完全相同的 artifact 配置不能重复。

本节只适用于自动更新工具；`release.type: manual` 不配置 artifacts。

### 5.1 类型兼容表

| artifact 类型 | GitHub | Web | HTTP | 说明 |
| --- | :---: | :---: | :---: | --- |
| `github-asset` | 是 | 否 | 否 | 下载第一个匹配的 release asset。 |
| `github-assets` | 是 | 否 | 否 | 下载所有匹配的 release assets。 |
| `github-source` | 是 | 否 | 否 | 下载 release tag 对应的源码包。 |
| `page-link` | 否 | 是 | 否 | 从 Web 页面的链接正则中提取 URL。 |
| `direct-url` | 是 | 是 | 是 | 下载固定 URL。 |
| `url-template` | 是 | 是 | 是 | 用解析到的版本替换 `{version}` 后下载。 |
| `release-url` | 否 | 否 | 是 | 直接下载 HTTP release 的 URL。 |

### 5.2 github-asset 与 github-assets

单文件匹配：

```yaml
artifacts:
  - type: github-asset
    pattern: '^frp_[^/]+_windows_amd64\.zip$'
```

多文件匹配：

```yaml
artifacts:
  - type: github-assets
    pattern: '^frida-server-.+\.xz$'
```

`pattern` 匹配 GitHub asset 的完整文件名。建议使用 `^` 和 `$` 锚定，并对 `.` 写成 `\.`，避免误匹配。`github-asset` 只取第一个匹配项；需要保存同一 release 的多个平台文件时使用 `github-assets`。

### 5.3 github-source

```yaml
artifacts:
  - type: github-source
    format: tar.gz
```

`format` 只支持 `zip` 或 `tar.gz`。

### 5.4 page-link

```yaml
artifacts:
  - type: page-link
    pattern: 'href="([^"]*tool-[^"]*\.zip)"'
```

`pattern` 如果包含捕获组，则使用第一个捕获组；否则使用完整匹配。省略 `base_url` 时，提取值会相对 `release.url` 按标准 URL 规则解析。

只有网页返回的是不适合标准 URL join 的片段时才指定 `base_url`：

```yaml
artifacts:
  - type: page-link
    pattern: 'data-file="([^"]+\.7z)"'
    base_url: https://cdn.example.com/files/
```

设置 `base_url` 后 updater 会直接拼接 `base_url` 与匹配值，因此必须自行保证斜杠边界正确。

### 5.5 direct-url、url-template 与 release-url

```yaml
artifacts:
  - type: direct-url
    url: https://example.com/runtime/helper.zip

  - type: url-template
    url: 'https://example.com/releases/{version}/tool-{version}.zip'
```

`url-template` 必须包含 `{version}`，且不允许其他占位符。所有 URL（含 `page-link` 的 `base_url`）都必须是带主机名的 HTTPS URL；仅当当前工具设置 `allow_insecure_transports: true` 时才允许 HTTP。

HTTP release 的原始文件可以写为：

```yaml
artifacts:
  - type: release-url
```

## 6. install：安装与保存策略

最小配置只需要目标路径，其余字段继承 manifest 默认值：

```yaml
install:
  destination: Web/Scanner/Nuclei
```

完整字段：

| 字段 | 可选值/类型 | 说明 |
| --- | --- | --- |
| `destination` | path | 必填。相对路径以 `toolkit_root` 为根。 |
| `input` | `extract` / `copy` | 解压产物或原样复制产物。 |
| `existing` | `replace` / `merge` | 替换已有内容或先合并已有内容。 |
| `save` | `directory` / `archive` | 保存为目录或重新压缩为 7z。 |
| `strip_single_root` | boolean | 只有一个根目录时是否提升内容。 |
| `create_destination` | boolean | 覆盖全局的目标创建策略。 |
| `archive_name` | string | 7z 输出文件名模板。 |
| `archive_password` | string | 解压加密输入包的密码，不能为空。 |
| `executable` | path list | 安装后必须存在的相对文件；Unix 上添加执行位。 |
| `allow_symlinks_in_archive` | boolean | 默认 `false`。是否允许输入压缩包内的 symlink/hardlink 条目（见 6.5）。 |
| `symlinks` | mapping list | 安装完成后创建的符号链接。 |

### 6.1 input

`input: extract` 会解压受支持的压缩包；如果下载文件不是压缩包，则按普通文件复制到组合目录。

`input: copy` 会保留下载文件本身，并自动把实际目标设为 `<destination>/release`。配置中的 `destination` 不能手动以 `release` 结尾：

```yaml
install:
  destination: Reverse/Hook/Frida
  input: copy
```

上例最终文件位于 `Reverse/Hook/Frida/release`，版本标记位于 `Reverse/Hook/Frida/.version`。

### 6.2 save

`save: directory` 将处理后的内容保存为目录，并写入 `.version`：

- `input: extract`：版本位于 `<destination>/.version`。
- `input: copy`：文件位于 `<destination>/release`，版本位于 `<destination>/.version`。

`save: archive` 会在解压、hook 和合并完成后使用 Rust 原生 7z 后端重新压缩：

```yaml
install:
  destination: Web/Scanner/MyTool
  save: archive
  archive_name: '{name}#{version}.7z'
```

此时 `destination` 是保存生成压缩包的目录。压缩包名称必须是跨平台安全文件名并以 `.7z` 结尾。

有一个刻意保留的特例：当 `input: copy` 且 artifacts 中包含 `github-asset`、`github-assets` 或 `github-source` 时，updater 会始终按 `directory` 输出。GitHub 原始发布文件直接保存到 `<destination>/release`，不会再次压缩，即使全局设置了 `save: archive`。

### 6.3 existing 与 strip_single_root

- `existing: replace`：成功准备新版本后原子替换旧内容。
- `existing: merge`：先把旧内容加入安装事务，再用新内容覆盖同名项。
- `strip_single_root: true`：某个解压结果只有一个顶层目录时，安装该目录的内容而不是目录本身。

不要在两个工具之间配置相同或互相包含的 destination。updater 会以不区分大小写的方式检查工具 destination、`.version`、符号链接目标和事务备份路径是否冲突。

### 6.4 executable 与 symlinks

```yaml
install:
  destination: Develop/Runtime/MyTool
  executable:
    - bin/my-tool
  symlinks:
    - from: bin/my-tool
      to: bin/my-tool
```

- `executable` 和 `symlinks.from` 必须是安装内容内的安全相对路径，不能包含 `..`。
- 相对的 `symlinks.to` 以 `toolkit_root` 为基准；也允许绝对目标。
- 符号链接目标不能重复、不能与自身源路径相同，也不能和其他工具管理的路径重叠。
- `save: archive` 不能配置 `symlinks`。
- `input: copy` 不能配置 `archive_password`。

### 6.5 allow_symlinks_in_archive

解压 tar 与 RAR 时，包含 symlink 或 hardlink 条目的压缩包默认整体拒绝（错误信息附带工具 ID 与条目名），防止恶意归档植入指向系统目录的链接。确有需要时按工具显式开启：

```yaml
install:
  destination: Tools/LinkedTool
  allow_symlinks_in_archive: true
```

开启后仍有两条硬性约束，违反即拒绝：link 目标必须是相对路径；目标与条目位置拼接后（存在则 canonicalize，不存在则按文本归一化）必须仍在解压目录内。绝对目标或指向解压目录外部的目标一律拒绝。此外，`strip_single_root` 单根提升与多产物合并会改变链接的相对边界，因此 opt-in 放行过链接时，安装会在组装完成后以最终安装目录为根复验全部 symlink，目标越出最终安装目录即整体失败。

安全边界说明：

- RAR 的 Unix symlink、Windows symlink、junction（目录联接）与 hardlink 均按链接条目处理；RAR3 中目标位于成员载荷、头部无目标信息的 symlink 即使开启开关也保持拒绝。
- 7z 后端无链接创建面（条目一律按普通文件提取），zip 后端本身不携带链接条目，均不受该开关影响。
- Windows 上 tar symlink 会在普通条目全部落盘后按目标实际类型创建（目录用目录链接，其余用文件链接），不依赖目标与链接在归档中的先后顺序；目标最终仍不存在时按文件链接创建，因此目录型悬空链接不受支持。
- 该开关只放宽"链接条目是否存在"，不会放宽上述界内约束；嵌套归档（解包后再解一层）沿用同一策略。

## 7. hooks：跨平台后处理

Hook 阶段按列表顺序执行：

```yaml
hooks:
  before_update: []
  after_unpack: []
  after_install: []
```

优先使用 Rust 原生 action。只有无法用原生 action 表达的逻辑才使用 Python 3；配置不支持 Shell、Batch、PowerShell 或任意外部可执行文件 action。

### 7.1 rename

`rename` 仅允许用于 `after_unpack`。它会在当前 artifact 的解压目录中递归查找文件名，且必须恰好匹配一个文件：

```yaml
hooks:
  after_unpack:
    - type: rename
      from: 'JDumpSpider*.jar'
      to: JDumpSpider.jar
```

`from` 是跨平台文件名通配模式，支持 `*` 和 `?`；`to` 只能是文件名，不能是路径。目标已存在时不会覆盖。

### 7.2 move-contents

`move-contents` 仅允许用于 `after_unpack`，来源和目标都相对当前 artifact 的解压目录：

```yaml
hooks:
  after_unpack:
    - type: move-contents
      from: release
      to: .
```

`from` 不能为 `.`，目标不能位于来源目录内部，且不能覆盖已有文件。

### 7.3 python

Python action 可用于三个阶段：

```yaml
hooks:
  after_unpack:
    - type: python
      script: scripts/custom/normalize.py
      args:
        - --strict
      timeout_seconds: 60
      working_directory: staging
      environment:
        MODE: portable
```

| 字段 | 必填 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `script` | 是 | 无 | 相对 updater 启动工作目录的安全 `.py` 路径；加载配置时文件必须存在。 |
| `args` | 否 | `[]` | 直接传递给脚本的参数列表，不经过 Shell。 |
| `timeout_seconds` | 否 | `300` | 必须大于 `0`。 |
| `working_directory` | 否 | `toolkit` | `app`、`toolkit`、`downloads`、`staging` 或 `install`。 |
| `environment` | 否 | `{}` | 额外环境变量。 |

`working_directory: staging` 只允许用于 `after_unpack`。脚本会收到以下保留环境变量，配置中的 `environment` 不允许覆盖任何以 `UTU_` 开头的变量：

- `UTU_TOOL_ID`
- `UTU_TOOL_NAME`
- `UTU_TOOLKIT_ROOT`
- `UTU_DOWNLOAD_DIR`
- `UTU_INSTALL_DIR`
- `UTU_STAGING_DIR`（当前阶段可用时）
- `UTU_VERSION`（版本已解析时）

解释器查找顺序：

- Windows：`py -3`、`python3`、`python`
- Linux/macOS：`python3`、`python`
- 设置 `UTU_PYTHON` 可以指定 Python 3 解释器路径。

## 8. 支持的压缩格式

`input: extract` 支持以下输入格式：

- `.zip`
- `.7z`
- `.rar`
- `.tar.gz`、`.tgz`
- `.tar.bz2`、`.tbz`
- `.tar.xz`、`.txz`
- `.gz`
- `.xz`

解压由 Rust 实现，不依赖 `unrar.exe`。`save: archive` 的输出格式当前固定为 `.7z`。

## 9. 完整示例

可以直接复制并修改的完整配置见 [`examples/manifest.yaml`](../examples/manifest.yaml) 和 [`examples/profile.yaml`](../examples/profile.yaml)。示例不会被默认 `profiles/manifest.yaml` 加载，其中的工具也全部设置为 `enabled: false`。

### 9.1 GitHub 单资产，解压为目录

```yaml
tools:
  d-beaver:
    name: DBeaver
    release:
      type: github
      repository: dbeaver/dbeaver
    artifacts:
      - type: github-asset
        pattern: '^dbeaver-ce-[^-]+-win32\.win32\.x86_64\.zip$'
    install:
      destination: Develop/Database/DBeaver
```

### 9.2 GitHub 多资产，保留原始发布文件

```yaml
tools:
  frida:
    name: Frida
    release:
      type: github
      repository: frida/frida
    artifacts:
      - type: github-assets
        pattern: '^frida-server-.+\.xz$'
    install:
      destination: Reverse/Hook/Frida
      input: copy
```

### 9.3 Web 页面解析版本与下载链接

```yaml
tools:
  example-tool:
    name: ExampleTool
    release:
      type: web
      url: https://example.com/downloads
      version_pattern: 'Latest version:\s*([0-9]+\.[0-9]+\.[0-9]+)'
    artifacts:
      - type: page-link
        pattern: 'href="([^"]*example-tool-win64\.zip)"'
    install:
      destination: Tools/ExampleTool
```

### 9.4 HTTP 固定地址，保留原文件

```yaml
tools:
  example-runtime:
    name: ExampleRuntime
    release:
      type: http
      url: https://example.com/releases/runtime-latest.zip
    artifacts:
      - type: release-url
    install:
      destination: Develop/Runtime/ExampleRuntime
      input: copy
```

## 10. 校验与排错

默认校验 `profiles/manifest.yaml`：

```bash
updater check
```

校验指定 profiles 目录：

```bash
updater --profiles /path/to/profiles check
```

校验指定 manifest：

```bash
updater --manifest /path/to/manifest.yaml check
```

`check` 不访问网络，但会检查：

- YAML 类型、未知字段和必填字段；
- schema 版本、include 文件和全局唯一 ID；
- URL、正则表达式、枚举值和占位符；
- release 与 artifact 类型兼容性；
- 安装路径、版本文件、符号链接和保留路径冲突；
- Hook 阶段限制、Python 脚本路径和环境变量。

提交配置前建议同时执行一次仅解析远端版本、不下载安装的计划：

```bash
updater update <tool-id> --dry-run
```

常见错误及处理方式：

| 错误 | 原因 | 修复 |
| --- | --- | --- |
| `unknown field` | 字段拼写错误或使用了旧 schema 字段。 | 按本文档字段名修改，不依赖兼容字段。 |
| `no GitHub asset matched` | asset 正则与当前 release 文件名不一致。 | 到 release 核对完整文件名，收紧或修正正则。 |
| `version regex needs a capture group` | Web 版本正则没有捕获组。 | 用括号捕获真正的版本部分。 |
| `destination must not end with 'release'` | `input: copy` 时手动写了 release。 | 从 destination 删除末尾 `release`。 |
| `non-portable filename` | 名称含跨平台非法字符，或 `#` 未正确引用。 | 使用安全名称，并给包含 `#` 的模板加引号。 |
| `overlapping destinations` | 两个工具管理了相同或父子路径。 | 按分类为每个工具分配独立 destination。 |

## 11. 提交前检查清单

- `schema_version` 为 `5`，新增 profile 已加入 `include`。
- 工具 ID 使用小写 kebab-case，且没有与其他 profile 重复。
- 工具展示名和 destination 遵循 `<工具>@<作者>` 与分类目录约定。
- GitHub asset 正则使用当前真实文件名验证过，并尽量用 `^...$` 锚定。
- `input: copy` 的 destination 没有手动附加 `release`。
- 需要原始 GitHub 文件时使用 `input: copy`，不依赖二次压缩。
- 所有相对路径都不包含 `..`，工具间 destination 不重叠。
- 简单后处理使用 Rust 原生 action，Python 只用于复杂逻辑。
- `updater check` 通过；涉及远端解析的工具还应通过 `--dry-run`。
