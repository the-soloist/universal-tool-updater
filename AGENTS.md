# AGENTS

本仓库是基于 Rust 的跨平台命令行工具更新器。修改代码时优先保持现有模块边界、错误处理方式和测试风格，不要为了整理目录而进行无关重构。

## 项目约定

- `src/` 同时包含库和二进制入口；共享逻辑放在库模块，命令行启动和日志初始化位于 `src/main.rs`。
- `build.sh` 是本地和 CI 共用的发布构建入口，使用 `--locked --release`，不要绕过脚本添加另一套构建逻辑。
- 发布目标为 Linux `x86_64`/`aarch64` musl、Windows `x86_64` GNU/`aarch64` MSVC，以及 macOS `x86_64`/`aarch64`。macOS 分架构构建，不再生成 Universal 二进制。
- Linux ARM64 交叉构建使用 Zig 和 `cargo-zigbuild`；Windows ARM64 由 Windows CI 验证。除非明确要求，不要在 macOS 上强行链接 Windows ARM64。
- 自更新资产名称必须与 `.github/workflows/release.yml` 和 `src/self_update.rs` 的平台映射保持一致；修改平台时同步更新 README 和对应测试。

# CODING GUIDE

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:

- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:

- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:

- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:

- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:

```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

## 5. Commit Convention

提交信息使用以下格式：

```text
xxx(xxx): 中文标题

- 修改内容1
- 修改内容2
```

第一行使用英文类型和作用域，标题使用简洁中文；正文与标题之间保留一个空行，并用中文概括实际修改内容。

## 6. 本地验证与 CI

本地验证按改动范围选择最小必要命令，不默认重复完整 CI 矩阵：

- 本地验证命令默认在沙箱外执行；涉及本地监听端口、临时文件、网络请求或跨平台构建时，直接申请沙箱外权限，避免沙箱限制造成误判。
- 本地只运行当前操作系统可直接执行的测试；不为其他操作系统交叉编译或模拟运行测试，平台专属测试交由对应 CI runner 验证。
- 仅修改注释、文档或格式时，不运行测试；只需运行 `git diff --check`，必要时检查对应文件格式。
- 文档、脚本或 workflow 改动：运行 `git diff --check`，并对 YAML 或 shell 做语法检查。
- Rust 行为代码改动：先运行 `cargo fmt -- --check`；涉及逻辑、接口、并发或平台分支时，针对受影响的 crate/测试运行 Clippy 和测试。例如日志入口可运行 `cargo clippy --bin updater -- -D warnings` 与 `cargo test --bin updater logging::tests`；库模块可使用 `cargo clippy --lib --all-features -- -D warnings` 和 `cargo test --lib <测试过滤器>`。仅注释或格式改动不运行 Clippy 和测试。
- 构建目标或构建脚本改动：优先验证 macOS ARM64：

  ```bash
  ./build.sh --target aarch64-apple-darwin
  ```

  Linux/Windows 其他目标由 CI 矩阵负责交叉构建验证。

只有用户明确要求时，才在本地运行完整测试：

```bash
cargo test --all-targets --all-features
```

CI 会在每个目标系统上执行完整的格式检查、Clippy、测试和 release 构建；提交前至少完成与改动相关的本地检查，并在最终提交信息中说明未运行的检查及原因。

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.
