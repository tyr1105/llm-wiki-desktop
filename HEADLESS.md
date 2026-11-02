# LLM Wiki Desktop 无窗口后台分支（headless）

[English summary](#english-summary)

这个分支基于 [StoneLL1/llm-wiki-desktop](https://github.com/StoneLL1/llm-wiki-desktop) 的 `app-v2.0.6`，加了 7 个提交，让桌面版的后台可以**不开窗口**运行，并通过本机 HTTP 接口被其他程序调用。它是 [xhs-wiki](https://github.com/tyr1105/xhs-wiki) 的"LLM Wiki 引擎"所用的后台程序。

> 这不是 LLM Wiki Desktop 的官方版本，和上游作者没有关联。想用桌面版，请到[上游的发布页](https://github.com/StoneLL1/llm-wiki-desktop/releases)下载。

## 下载

- 在本仓库的 [Releases](https://github.com/tyr1105/llm-wiki-desktop/releases) 下载 `llm-wiki-headless-<版本>-aarch64-apple-darwin.tar.gz`，**只支持 Apple 芯片的 Mac**。
- 程序由本仓库的 GitHub Actions（[headless-release.yml](.github/workflows/headless-release.yml)）在干净的 macOS 构建机上编译，不是在个人电脑上编译的。每个附件都附带 SHA-256，并带有 GitHub 的构建来源证明，可以这样核对：

  ```bash
  shasum -a 256 llm-wiki-headless-<版本>-aarch64-apple-darwin.tar.gz
  gh attestation verify llm-wiki-headless-<版本>-aarch64-apple-darwin.tar.gz --repo tyr1105/llm-wiki-desktop
  ```

- 程序只做了临时签名（ad-hoc），没有经过苹果公证。用 `curl` 下载不会被 macOS 拦截（xhs-wiki 就是这样下载的）；用浏览器下载的话，系统可能提示"无法验证开发者"。

## 用法

```bash
export LLM_WIKI_API_TOKEN="$(openssl rand -hex 32)"   # 64 位十六进制令牌，只放在环境变量里
llm-wiki-desktop --serve-api --headless --api-port 18766 \
  --project-root /知识库/所在目录 --import-root /待导入文件/所在目录
```

- 接口只监听 `127.0.0.1`，每个请求都要带 `Authorization: Bearer <令牌>`；POST 请求必须带 `Idempotency-Key`。完整说明见 [docs/local-api.md](docs/local-api.md) 和 [docs/local-api.openapi.json](docs/local-api.openapi.json)。
- `--project-root` 和 `--import-root` 是启动时冻结的允许清单，接口只能打开和读取这两个目录里的内容。
- 后台和桌面版共用同一个单实例锁和配置目录（`~/.config/llm-wiki-desktop/`）。桌面版开着的时候，后台会立即退出；后台运行期间，桌面版也打不开。
- 无窗口模式下菜单栏仍会出现 LLM Wiki 图标。整理进行中请不要点图标菜单里的"退出"。
- 整理用的 Agent（Claude Code 或 Codex）从 `PATH` 里查找；后台会去掉子进程的大部分环境变量，调用方需要自己保证 Agent 能找到登录信息（xhs-wiki 用包装脚本处理）。

## 改了什么

| 提交 | 内容 |
|---|---|
| 新增本地自动化接口与无窗口模式（--serve-api --headless） | 本机 HTTP 接口：打开知识库、导入、Update Wiki、读候选差异、确认或取消，复用界面同一套权限和检查点校验；令牌、Origin/Host 校验、幂等键、请求体与并发上限；`--headless` 不建窗口；与桌面版共用单实例锁；Agent 路线按计划逐页生成 |
| Update Wiki 按 Agent 设输入预算，超出时改走有界规划 | 按 Claude / Codex 分别计算上下文窗口和输入上限（`LLM_WIKI_AGENT_CONTEXT_TOKENS` 可覆盖），超阈值时由模型用只读工具按需读取页面 |
| 新增 Claude 只读整理调用，页面生成按预算分层传入上下文 | Claude 整理只开 Read / Grep / Glob 并带 `--restricted`，启动前按 `--help` 核对参数 |
| 日志止涨与结构页增长闸门 | 完整审计记录写进 `.app/compile-audit/`，`wiki/log.md` 每批只追加摘要；index / overview 增长超限时报错 |
| 细分 Claude 和 Codex 调用失败的原因 | 区分上下文超限、额度用完、未登录，写进 `AGENT_EXIT_FAILED` 的 `agentFailureCategory` |
| 修正上下文预算默认值和结构页闸门规则 | Claude 默认窗口 100 万 token；小页豁免、按来源数放宽增量、绝对体积上限 |
| 派生专题页分批并行生成 | 每批最多 3 路并行（`LLM_WIKI_PAGE_PARALLELISM` 可设 1~4），一页失败即停下同批其他调用 |

代码注释里的"V7""V8""V8.1"是这些补丁的批次名：V7 对应第 1 个提交，V8 对应第 2～6 个，V8.1 对应第 7 个。

<details>
<summary>新增和修改的文件（Apache-2.0 第 4(b) 条要求的改动说明）</summary>

| 文件 | 改动 |
|---|---|
| `docs/local-api.md` | 新增 |
| `docs/local-api.openapi.json` | 新增 |
| `release/command-authority-inventory.json` | 修改 |
| `src-tauri/Cargo.lock` | 修改 |
| `src-tauri/Cargo.toml` | 修改 |
| `src-tauri/src/commands/compile_commands.rs` | 修改 |
| `src-tauri/src/commands/import_v2_commands.rs` | 修改 |
| `src-tauri/src/commands/import_v2_file_commands.rs` | 修改 |
| `src-tauri/src/lib.rs` | 修改 |
| `src-tauri/src/local_api/config.rs` | 新增 |
| `src-tauri/src/local_api/dispatch.rs` | 新增 |
| `src-tauri/src/local_api/mod.rs` | 新增 |
| `src-tauri/src/local_api/retry.rs` | 新增 |
| `src-tauri/src/local_api/scope.rs` | 新增 |
| `src-tauri/src/local_api/security.rs` | 新增 |
| `src-tauri/src/models/compile.rs` | 修改 |
| `src-tauri/src/services/agent_service.rs` | 修改 |
| `src-tauri/src/services/compile_instructions.rs` | 修改 |
| `src-tauri/src/services/compile_service.rs` | 修改 |
| `src-tauri/src/services/compile_service/agent_budget.rs` | 新增 |
| `src-tauri/src/services/compile_service/agent_pages.rs` | 新增 |
| `src-tauri/src/services/compile_service/agent_plan.rs` | 新增 |
| `src-tauri/src/services/compile_service/source_context.rs` | 修改 |
| `src-tauri/src/services/import_execution.rs` | 修改 |
| `src-tauri/src/services/import_v2/file_discovery.rs` | 修改 |
| `src-tauri/src/services/mod.rs` | 修改 |
| `src-tauri/src/services/settings_service.rs` | 修改 |
| `src-tauri/src/services/workflow_service/runners/update_wiki.rs` | 修改 |
| `src-tauri/src/services/workflow_service/runners/update_wiki/history.rs` | 修改 |
| `src-tauri/src/services/workflow_service/update_intent.rs` | 修改 |
| `src-tauri/src/services/workflow_service/update_intent/tests.rs` | 修改 |
| `src-tauri/src/utils/markdown_utils.rs` | 修改 |
| `src-tauri/tests/import_v2_file_orchestration.rs` | 修改 |
| `src-tauri/tests/mvp_flow.rs` | 修改 |
| `src-tauri/tests/task8_contracts.rs` | 修改 |
| `src-tauri/tests/workflow_routes.rs` | 修改 |
| `src-tauri/tests/workflow_update_wiki.rs` | 修改 |

另外新增了本文件、`NOTICE` 和 `.github/workflows/headless-release.yml`，并在两份 README 开头加了分支说明。

</details>

## 自己编译

需要 Rust 1.92.0（和上游 CI 相同）：

```bash
cargo build --release --locked --no-default-features --features local-api \
  --manifest-path src-tauri/Cargo.toml --bin llm-wiki-desktop
```

产物在 `src-tauri/target/release/llm-wiki-desktop`。注意：本机编译的程序会带上本机的目录名和用户名，不要拿去分发。

## 许可证

Apache License 2.0，见 [LICENSE](LICENSE) 和 [NOTICE](NOTICE)。上游代码的版权归 LLM Wiki Desktop Contributors；本分支的改动同样以 Apache-2.0 发布。

---

## English summary

This `headless` branch is based on upstream `app-v2.0.6` of [StoneLL1/llm-wiki-desktop](https://github.com/StoneLL1/llm-wiki-desktop) plus 7 commits: a loopback-only automation API with a windowless mode (`--serve-api --headless`), per-agent context budgets with bounded planning, read-only Claude compile invocations, compile audit records and structural page growth limits, agent failure classification, and parallel generation of derived pages. It is the backend used by the LLM Wiki engine of [xhs-wiki](https://github.com/tyr1105/xhs-wiki).

It is **not** an official LLM Wiki Desktop release and is not affiliated with the upstream authors. Apple Silicon binaries are built by GitHub Actions on clean runners and published on the [Releases](https://github.com/tyr1105/llm-wiki-desktop/releases) page with SHA-256 checksums and build provenance attestations. The API is documented in [docs/local-api.md](docs/local-api.md). Licensed under Apache-2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE).
