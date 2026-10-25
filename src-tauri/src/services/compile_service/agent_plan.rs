//! Planning context for a read-only Agent; complete files remain in its workspace.
//!
//! V8 规则（做什么、为什么）：
//! * 所有具备只读文件工具的 Agent（Codex、Claude）在全量规划提示超过该 Agent 的
//!   内联阈值时，都改走有界规划：只内嵌 purpose/schema、本组来源全文和现有页面
//!   清单（路径、哈希、大小），页面正文留在工作区里让模型按需读取。V7 只给 Codex
//!   开了这条路，Claude 被写死拿全量，现有知识库规模下一启动就超限秒败。
//! * 任何路径（全量和有界）都不把 wiki/log.md 的正文放进规划提示：日志 120 万字、
//!   每批还在涨，规划根本用不到它；清单里只保留路径并标注
//!   retained_history_local_append_only。
//! * 页面清单超过 64KB 时写成工作区文件，提示里只给路径，保证页面再多初始输入也有上界。
//! * 有界提示仍超过硬上限时，在调用命令行之前就报 COMPILE_AGENT_INPUT_TOO_LARGE，
//!   details.reason 写明是"所选来源本身太大"还是"固定部分太大"。
use super::*;
use super::agent_budget::{self, AgentInputBudget, OversizeReason};

/// 页面清单内嵌上限（UTF-8 字节）。超过就写成工作区文件，只在提示里给路径。
pub(super) const INVENTORY_INLINE_MAX_BYTES: usize = 64 * 1024;
/// 页面清单文件名：放在候选工作区根目录，和 `.compile-source-inputs.json` 同一约定，
/// 模型用只读工具（Read/Grep）就能查到。
pub(super) const WORKSPACE_WIKI_INVENTORY: &str = ".compile-wiki-inventory.json";

/// 规划提示走了哪条路，用于任务日志和测试断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanningMode {
    /// 全量内联：工作区页面整篇内嵌（log.md 正文除外）。
    FullInline,
    /// 有界：来源全文 + 页面清单；`inventory_in_file` 表示清单是否写成了工作区文件。
    Bounded { inventory_in_file: bool },
}

/// 规划提示及其预算信息。调用方用 `text` 调命令行，用 `log_line()` 写任务日志。
#[derive(Debug, Clone)]
pub(super) struct PlanningPrompt {
    pub(super) text: String,
    pub(super) mode: PlanningMode,
    pub(super) budget: AgentInputBudget,
}

impl PlanningPrompt {
    /// 一行任务日志：走了哪条路、估算多少 token、预算是多少。只含计数，不含正文。
    pub(super) fn log_line(&self) -> String {
        let route = match self.mode {
            PlanningMode::FullInline => {
                "complete workspace embedded inline (wiki/log.md body omitted)".to_string()
            }
            PlanningMode::Bounded { inventory_in_file } => format!(
                "bounded (selected Sources in full + Wiki page inventory{}; page bodies and wiki/log.md not embedded)",
                if inventory_in_file {
                    format!(" in {WORKSPACE_WIKI_INVENTORY}")
                } else {
                    String::new()
                }
            ),
        };
        format!(
            "Planning input for {}: {route}; ~{} tokens (UTF-8 bytes / 2), {} UTF-16 units; {}",
            self.budget.agent.command(),
            agent_budget::estimate_tokens(&self.text),
            agent_budget::character_units(&self.text),
            self.budget.describe(),
        )
    }
}

/// 生产入口：按该 Agent 的预算（含环境变量覆盖）构建规划提示。
pub(super) fn prompt(
    agent: AgentKind,
    workspace: &Path,
    language: &str,
    policy: CompileGenerationPolicy,
) -> Result<PlanningPrompt, BackendError> {
    prompt_with_budget(AgentInputBudget::for_agent(agent), workspace, language, policy)
}

/// 按给定预算构建规划提示。拆出来是为了测试能注入不同窗口验证切换逻辑。
pub(super) fn prompt_with_budget(
    budget: AgentInputBudget,
    workspace: &Path,
    language: &str,
    policy: CompileGenerationPolicy,
) -> Result<PlanningPrompt, BackendError> {
    // 全量提示：工作区页面整篇内嵌，但 wiki/log.md 只留一行占位说明
    // （由 append_workspace_markdown 的规划模式负责），所以全量路径同样不含日志正文。
    let full = CompileService::provider_plan_prompt_with_policy(workspace, language, policy)?;
    let full_details = serde_json::json!({"stage": "planning", "planningMode": "full_inline"});
    // 没有只读工具的 Agent 不能"按需读文件"，只能拿全量；有工具但全量足够小的，
    // 也照旧内联（小知识库行为不变）。两种情况都要过硬上限预检。
    if !budget.read_only_tools || budget.fits_inline(&full) {
        budget.validate_with_reason(&full, OversizeReason::FullWorkspace, full_details)?;
        remove_stale_inventory(workspace);
        return Ok(PlanningPrompt { text: full, mode: PlanningMode::FullInline, budget });
    }
    // Read-only workspace tools can retrieve exact existing pages; tool-disabled
    // profiles never reach this branch (see `budget.read_only_tools` above).
    let catalog_path = workspace.join(WORKSPACE_SOURCE_INPUTS);
    if !catalog_path.is_file() {
        budget.validate_with_reason(&full, OversizeReason::FullWorkspace, full_details)?;
        remove_stale_inventory(workspace);
        return Ok(PlanningPrompt { text: full, mode: PlanningMode::FullInline, budget });
    }
    let catalog_text = read_input(workspace, WORKSPACE_SOURCE_INPUTS, false)?;
    let catalog: Vec<WorkspaceSourceInput> = serde_json::from_str(&catalog_text)
        .map_err(|error| BackendError::new("COMPILE_INPUT_READ_FAILED", error.to_string(), true, false))?;
    let mut prompt = CompileService::plan_prompt_header_for_route(
        CompilePromptRoute::AgentReadOnlyPlan,
        language,
        policy,
    );
    prompt.push_str("\nBounded planning context: complete selected Source bodies and their exact version catalog follow. Existing Wiki page bodies are NOT embedded. They remain unchanged, complete, read-only files inside this isolated workspace. Use read-only file tools to inspect the exact listed existing page paths needed to choose targets, preserve prior knowledge/citations, and prove already_covered decisions. Never claim to have read omitted page bodies, never infer coverage from filenames or hashes, and never read outside this workspace. Read relevant pages in bounded sections if needed. If evidence cannot be read, defer with a reason instead of claiming coverage. Do not read or regenerate the full historical wiki/log.md: candidate generation appends a bounded entry locally while retaining the complete history. Return only CompilePlan JSON as your final response.\n");
    for name in ["purpose.md", "schema.md"] {
        let content = read_input(workspace, name, true)?;
        prompt.push_str(&format!("\n--- {name} ---\n{content}"));
    }
    prompt.push_str("\n--- Selected Source versions ---\nUse each listed path for CompilePlan sourceIds and Source citations. Registry IDs and version IDs are provenance only. These are the complete approved Sources; do not discover or add other Sources.\n");
    prompt.push_str(&catalog_text);
    let mut seen = HashSet::new();
    // 单独统计"所选来源全文"占了多少字节/字符：超限时据此判断是不是来源本身太大。
    let (mut source_bytes, mut source_units) = (0_usize, 0_usize);
    for source in &catalog {
        if seen.insert(source.path.clone()) {
            let content = read_input(workspace, &source.path, true)?;
            let section = format!("\n--- {} ---\n{content}", source.path);
            source_bytes += section.len();
            source_units += agent_budget::character_units(&section);
            prompt.push_str(&section);
        }
    }
    let mut files = FileStore.list_markdown_files(&workspace.join("wiki"))?;
    files.sort();
    let mut inventory = Vec::new();
    for file in files {
        let path = file.strip_prefix(workspace).map_err(|error| {
            BackendError::new("COMPILE_INPUT_PATH_INVALID", error.to_string(), true, false)
        })?.to_string_lossy().replace('\\', "/");
        if seen.contains(&path) || is_compile_protected_path(&path) {
            continue;
        }
        let content = read_input(workspace, &path, true)?;
        inventory.push(serde_json::json!({
            "path": path, "sha256": hash_bytes(content.as_bytes()),
            "bytes": content.len(), "characters": content.chars().count(),
            "bodyEmbedded": false,
            "planningUse": if is_historical_log_path(&path) { "retained_history_local_append_only" } else { "read_exact_file_as_needed" }
        }));
    }
    let inventory_text = serde_json::to_string_pretty(&inventory).map_err(|error| {
        BackendError::new("COMPILE_INPUT_READ_FAILED", error.to_string(), true, false)
    })?;
    let inventory_in_file = inventory_text.len() > INVENTORY_INLINE_MAX_BYTES;
    if inventory_in_file {
        // 清单太大：写成工作区里的只读文件，提示里只给路径、条数和完整性信息。
        write_inventory_file(workspace, &inventory_text)?;
        prompt.push_str(&format!(
            "\n--- Existing Wiki file inventory; bodies omitted, not deleted or rewritten ---\nThe inventory of {} existing Wiki files is too large to embed. It is the read-only workspace file `{WORKSPACE_WIKI_INVENTORY}` ({} UTF-8 bytes, SHA-256 {}): a JSON array of {{path, sha256, bytes, characters, bodyEmbedded, planningUse}}. Use read-only Grep/Read on that file to find candidate page paths, then read only the exact pages you need. These are integrity metadata, not evidence that you have read it.\n",
            inventory.len(),
            inventory_text.len(),
            hash_bytes(inventory_text.as_bytes()),
        ));
    } else {
        remove_stale_inventory(workspace);
        prompt.push_str("\n--- Existing Wiki file inventory; bodies omitted, not deleted or rewritten ---\n");
        prompt.push_str(&inventory_text);
    }
    // 判断超限原因：去掉来源全文后若能放下，说明是"所选来源本身太大"，拆分组有用；
    // 否则是提示头/purpose/schema/清单这些固定部分太大，拆分组也没用。
    let fixed_bytes = prompt.len() - source_bytes;
    let fixed_units = agent_budget::character_units(&prompt) - source_units;
    let reason = if budget.exceeded_limits(fixed_bytes, fixed_units).is_empty() {
        OversizeReason::SelectedSources
    } else {
        OversizeReason::FixedContext
    };
    budget.validate_with_reason(&prompt, reason, serde_json::json!({
        "stage": "planning",
        "planningMode": "bounded",
        "selectedSourceCount": seen.len(),
        "selectedSourceBytes": source_bytes,
        "estimatedTokensWithoutSelectedSources": agent_budget::tokens_for_bytes(fixed_bytes),
        "inventoryInFile": inventory_in_file,
    }))?;
    Ok(PlanningPrompt { text: prompt, mode: PlanningMode::Bounded { inventory_in_file }, budget })
}

/// 页面生成、Legacy 清单等其他调用的通用预检：按该 Agent 的预算（Claude 也算），
/// 超过硬上限就在启动命令行之前报 COMPILE_AGENT_INPUT_TOO_LARGE。
pub(super) fn validate_input(agent: AgentKind, prompt: &str) -> Result<(), BackendError> {
    AgentInputBudget::for_agent(agent).validate(prompt)
}

/// 安全地写页面清单文件：先删掉同名旧文件（含符号链接），再用 create_new 新建，
/// 避免顺着工作区里意外存在的链接写到别处。权限沿用工作区的私有目录。
fn write_inventory_file(workspace: &Path, text: &str) -> Result<(), BackendError> {
    let path = workspace.join(WORKSPACE_WIKI_INVENTORY);
    if std::fs::symlink_metadata(&path).is_ok() {
        std::fs::remove_file(&path).map_err(|error| io_error("COMPILE_WORKSPACE_FAILED", error, &path))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut handle = options.open(&path).map_err(|error| io_error("COMPILE_WORKSPACE_FAILED", error, &path))?;
    handle.write_all(text.as_bytes()).map_err(|error| io_error("COMPILE_WORKSPACE_FAILED", error, &path))
}

/// 本次没有用到清单文件时，顺手删掉上一次规划留下的旧文件，免得模型读到过期清单。
/// 删除失败不影响本次规划（提示里不会引用它），所以忽略错误。
fn remove_stale_inventory(workspace: &Path) {
    let path = workspace.join(WORKSPACE_WIKI_INVENTORY);
    if std::fs::symlink_metadata(&path).is_ok() {
        let _ = std::fs::remove_file(&path);
    }
}

fn read_input(workspace: &Path, path: &str, markdown: bool) -> Result<String, BackendError> {
    let relative = Path::new(path);
    if relative.as_os_str().is_empty()
        || relative.components().any(|part| !matches!(part, Component::Normal(_)))
        || (markdown && relative.extension().and_then(|ext| ext.to_str()).is_none_or(|ext| !ext.eq_ignore_ascii_case("md")))
    {
        return Err(BackendError::new("COMPILE_INPUT_PATH_INVALID", "Planning input must be a relative file inside the candidate workspace.", true, false));
    }
    let root = std::fs::canonicalize(workspace).map_err(|error| io_error("COMPILE_INPUT_READ_FAILED", error, workspace))?;
    let file = workspace.join(relative);
    let canonical = std::fs::canonicalize(&file).map_err(|error| io_error("COMPILE_INPUT_READ_FAILED", error, &file))?;
    if !canonical.starts_with(&root) || !canonical.is_file() {
        return Err(BackendError::new("COMPILE_INPUT_PATH_INVALID", "Planning input leaves the candidate workspace.", true, false));
    }
    std::fs::read_to_string(&canonical).map_err(|error| io_error("COMPILE_INPUT_READ_FAILED", error, &canonical))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::agent_budget::CODEX_MAX_INPUT_CHARACTERS;

    const POLICY: CompileGenerationPolicy = CompileGenerationPolicy::WorkflowReviewableDeletes;
    const SOURCE_PATH: &str = "raw/sources/中文 资料/原文.md";
    const SOURCE_BODY: &str = "# 来源\n完整事实和引用 ORIGINAL_SOURCE_END";

    /// 测试工作区：purpose/schema、一条所选来源（含版本目录）、log.md、index 和一个已有页面。
    /// `page_body` 决定全量提示有多大；`history` 是日志正文（断言它永远不进规划提示）。
    fn workspace_fixture(page_body: &str, history: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let w = root.path();
        std::fs::create_dir_all(w.join("wiki")).unwrap();
        std::fs::create_dir_all(w.join("raw/sources/中文 资料")).unwrap();
        std::fs::write(w.join("purpose.md"), "# Purpose\nPreserve knowledge").unwrap();
        std::fs::write(w.join("schema.md"), "# Schema\nExact source citations").unwrap();
        std::fs::write(w.join(SOURCE_PATH), SOURCE_BODY).unwrap();
        let catalog = vec![WorkspaceSourceInput {
            registry_source_id: "source-1".into(),
            version_id: "version-1".into(),
            content_hash: hash_bytes(SOURCE_BODY.as_bytes()),
            path: SOURCE_PATH.into(),
        }];
        std::fs::write(w.join(WORKSPACE_SOURCE_INPUTS), serde_json::to_vec(&catalog).unwrap()).unwrap();
        std::fs::write(w.join("wiki/log.md"), history).unwrap();
        std::fs::write(w.join("wiki/index.md"), "# Index\n[[中文 页面]]").unwrap();
        std::fs::write(w.join("wiki/中文 页面.md"), page_body).unwrap();
        root
    }

    /// 约 1.65MB、125 万字符的已有页面：超过 Claude/Codex 的内联阈值和 Codex 的 CLI 字符上限，
    /// 复现真实知识库"全量规划提示过大"的情形。
    fn large_page() -> String {
        format!("# Existing\nUSER_EDIT_MUST_SURVIVE\n{}", "EXISTING_PAGE_BODY 已有知识\n".repeat(50_000))
    }

    fn history() -> String {
        "HISTORICAL_LOG_RECORD\n".repeat(60_000)
    }

    #[test]
    fn large_workspace_planning_is_bounded_for_every_read_only_agent() {
        let root = workspace_fixture(&large_page(), &history());
        let w = root.path();
        let full = CompileService::provider_plan_prompt_with_policy(w, "zh-CN", POLICY).unwrap();
        assert!(full.chars().count() > CODEX_MAX_INPUT_CHARACTERS, "fixture must reproduce the actual oversized planning input");
        assert!(full.contains("EXISTING_PAGE_BODY"));
        assert!(!full.contains("HISTORICAL_LOG_RECORD"), "even the full prompt must omit the log body");
        assert!(full.contains("retained_history_local_append_only"));
        // V8：Claude 和 Codex 都具备只读工具，都改走有界规划（原来 Claude 被写死拿全量）。
        for agent in [AgentKind::Claude, AgentKind::Codex] {
            let planning = prompt(agent, w, "zh-CN", POLICY).unwrap();
            assert_eq!(planning.mode, PlanningMode::Bounded { inventory_in_file: false }, "{agent:?}");
            let bounded = &planning.text;
            let trusted_header = bounded.split("--- purpose.md ---").next().unwrap();
            assert!(!trusted_header.contains("you have no filesystem or tool access"), "bounded Agent planning must not inherit the tool-disabled BYOK contract");
            assert!(trusted_header.contains("Use read-only file tools"), "bounded Agent planning must explicitly allow evidence reads");
            assert!(trusted_header.contains("Never write, modify, or delete files"), "planning must remain read-only");
            assert!(trusted_header.contains("Never list wiki/log.md in affectedExistingPages"));
            assert!(bounded.chars().count() < 128_000, "large pages should be inventoried, not sent in full");
            assert!(bounded.contains(SOURCE_BODY));
            assert!(bounded.contains(SOURCE_PATH));
            assert!(bounded.contains("source-1") && bounded.contains("version-1"));
            assert!(bounded.contains("wiki/中文 页面.md"));
            assert!(bounded.contains(&hash_bytes(history().as_bytes())));
            assert!(!bounded.contains("HISTORICAL_LOG_RECORD"));
            assert!(!bounded.contains("EXISTING_PAGE_BODY"));
            assert!(planning.log_line().contains("bounded"));
            assert!(!planning.log_line().contains("EXISTING_PAGE_BODY"));
        }
        // 没有经过验证的只读配置的 Agent 不能拿"自己去读文件"的提示，仍然拿全量。
        for agent in [AgentKind::Openclaw, AgentKind::Hermes] {
            let planning = prompt(agent, w, "zh-CN", POLICY).unwrap();
            assert_eq!(planning.mode, PlanningMode::FullInline);
            assert_eq!(planning.text, full, "tool-disabled profiles must not receive a file-reading fallback");
        }
        assert_eq!(std::fs::read_to_string(w.join("wiki/log.md")).unwrap(), history());
        assert_eq!(std::fs::read_to_string(w.join(SOURCE_PATH)).unwrap(), SOURCE_BODY);
        assert_eq!(std::fs::read_to_string(w.join("wiki/中文 页面.md")).unwrap(), large_page());
    }

    #[test]
    fn oversized_selected_sources_fail_before_any_agent_launch() {
        let root = workspace_fixture(&large_page(), &history());
        let w = root.path();
        let oversized_source = format!("{}REQUIRED_SOURCE_END", "选".repeat(CODEX_MAX_INPUT_CHARACTERS));
        std::fs::write(w.join(SOURCE_PATH), &oversized_source).unwrap();
        for agent in [AgentKind::Claude, AgentKind::Codex] {
            let error = prompt(agent, w, "zh-CN", POLICY).unwrap_err();
            assert_eq!(error.code, "COMPILE_AGENT_INPUT_TOO_LARGE");
            let details = error.details.clone().unwrap();
            // 去掉来源后能放下 → 是"所选来源本身太大"，重试脚本据此拆分组。
            assert_eq!(details["reason"], "selected_sources", "{agent:?}");
            assert_eq!(details["stage"], "planning");
            assert_eq!(details["planningMode"], "bounded");
            assert_eq!(details["selectedSourceCount"], 1);
            assert_eq!(details["agent"], agent.command());
            assert!(!format!("{error:?}").contains("选选选"), "errors must contain counts, not private prompt bodies");
        }
        assert_eq!(std::fs::read_to_string(w.join(SOURCE_PATH)).unwrap(), oversized_source, "required Source must never be truncated to fit");
    }

    #[test]
    fn oversized_fixed_context_is_reported_separately_from_sources() {
        let root = workspace_fixture(&large_page(), &history());
        let w = root.path();
        // schema 本身约 65 万字节（约 32.5 万 token）。用 20 万窗口（硬上限 10 万 token）的
        // Claude 验证：拆分来源也没用，原因必须写成 fixed_context。
        std::fs::write(w.join("schema.md"), "# Schema\n".to_string() + &"规则说明\n".repeat(50_000)).unwrap();
        let narrow = AgentInputBudget::for_agent_with_overrides(AgentKind::Claude, Some("claude=200000"));
        let error = prompt_with_budget(narrow, w, "zh-CN", POLICY).unwrap_err();
        assert_eq!(error.code, "COMPILE_AGENT_INPUT_TOO_LARGE");
        assert_eq!(error.details.unwrap()["reason"], "fixed_context");
        // 默认窗口（100 万，硬上限 50 万 token）同样适用：schema 再大一些（约 130 万字节）就超。
        std::fs::write(w.join("schema.md"), "# Schema\n".to_string() + &"规则说明\n".repeat(100_000)).unwrap();
        let error = prompt(AgentKind::Claude, w, "zh-CN", POLICY).unwrap_err();
        assert_eq!(error.code, "COMPILE_AGENT_INPUT_TOO_LARGE");
        assert_eq!(error.details.unwrap()["reason"], "fixed_context");
    }

    #[test]
    fn planning_prompts_never_embed_the_historical_log_body() {
        // 小知识库（走全量内联）和大知识库（走有界）都要覆盖；每个 Agent 都检查。
        for page in ["# Small\nsmall page".to_string(), large_page()] {
            let root = workspace_fixture(&page, &history());
            for agent in AgentKind::ALL {
                let planning = prompt(agent, root.path(), "zh-CN", POLICY).unwrap();
                assert!(!planning.text.contains("HISTORICAL_LOG_RECORD"), "{agent:?} {:?}", planning.mode);
                assert!(planning.text.contains("wiki/log.md"), "the path stays visible with its retained-history annotation");
                assert!(planning.text.contains("retained_history_local_append_only"));
            }
        }
    }

    #[test]
    fn small_knowledge_base_stays_fully_inline_for_every_agent() {
        // 有来源清单也一样：小知识库不切换有界模式，提示与全量完全一致。
        let root = workspace_fixture("# Existing\nUSER_EDIT_MUST_SURVIVE", "small history");
        let full = CompileService::provider_plan_prompt_with_policy(root.path(), "zh-CN", POLICY).unwrap();
        assert!(full.contains("USER_EDIT_MUST_SURVIVE"));
        assert!(full.contains(SOURCE_BODY));
        for agent in AgentKind::ALL {
            let planning = prompt(agent, root.path(), "zh-CN", POLICY).unwrap();
            assert_eq!(planning.mode, PlanningMode::FullInline, "{agent:?}");
            assert_eq!(planning.text, full);
        }
        assert!(!root.path().join(WORKSPACE_WIKI_INVENTORY).exists());
    }

    #[test]
    fn small_planning_prompts_remain_identical() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki")).unwrap();
        std::fs::write(root.path().join("purpose.md"), "Purpose").unwrap();
        std::fs::write(root.path().join("schema.md"), "Schema").unwrap();
        std::fs::write(root.path().join("wiki/log.md"), "History").unwrap();
        let expected = CompileService::provider_plan_prompt_with_policy(root.path(), "en", POLICY).unwrap();
        assert_eq!(prompt(AgentKind::Codex, root.path(), "en", POLICY).unwrap().text, expected);
    }

    #[test]
    fn environment_override_changes_the_planning_decision() {
        let root = workspace_fixture(&large_page(), &history());
        let full = CompileService::provider_plan_prompt_with_policy(root.path(), "zh-CN", POLICY).unwrap();
        // 默认窗口（opus 别名实测的 100 万，内联阈值 20 万 token）：约 165 万字节的大知识库走有界。
        let default = AgentInputBudget::for_agent_with_overrides(AgentKind::Claude, None);
        assert!(matches!(prompt_with_budget(default, root.path(), "zh-CN", POLICY).unwrap().mode, PlanningMode::Bounded { .. }));
        // 用环境变量把 Claude 窗口调到 1000 万：内联阈值 200 万 token，同一份全量就能直接内联。
        let wide = AgentInputBudget::for_agent_with_overrides(AgentKind::Claude, Some("claude=10000000"));
        let planning = prompt_with_budget(wide, root.path(), "zh-CN", POLICY).unwrap();
        assert_eq!(planning.mode, PlanningMode::FullInline);
        assert_eq!(planning.text, full);
        assert!(planning.log_line().contains("env_override"));
        // Codex 即使窗口调大，CLI 字符上限仍然生效：125 万字符的全量不能内联。
        let wide_codex = AgentInputBudget::for_agent_with_overrides(AgentKind::Codex, Some("codex=10000000"));
        assert!(matches!(prompt_with_budget(wide_codex, root.path(), "zh-CN", POLICY).unwrap().mode, PlanningMode::Bounded { .. }));
    }

    #[test]
    fn large_page_inventory_is_written_to_a_workspace_file() {
        let root = workspace_fixture("# Existing\nsmall", "small history");
        let w = root.path();
        std::fs::create_dir_all(w.join("wiki/concepts")).unwrap();
        for index in 0..500 {
            std::fs::write(w.join(format!("wiki/concepts/页面-{index:04}.md")), format!("# 页面 {index}\n{}", "正文".repeat(60))).unwrap();
        }
        // 全量提示约 20 万字节：用 20 万窗口（内联阈值 4 万 token）的 Claude 触发有界规划，
        // 验证"清单超过 64KB 就写成文件"。默认 100 万窗口下这么小的库会直接全量内联。
        let narrow = || AgentInputBudget::for_agent_with_overrides(AgentKind::Claude, Some("claude=200000"));
        let planning = prompt_with_budget(narrow(), w, "zh-CN", POLICY).unwrap();
        assert_eq!(planning.mode, PlanningMode::Bounded { inventory_in_file: true });
        assert!(planning.text.contains(WORKSPACE_WIKI_INVENTORY));
        assert!(!planning.text.contains("wiki/concepts/页面-0001.md"), "a large inventory must not be embedded");
        assert!(planning.text.len() < INVENTORY_INLINE_MAX_BYTES, "initial input stays bounded however many pages exist");
        let file = std::fs::read_to_string(w.join(WORKSPACE_WIKI_INVENTORY)).unwrap();
        assert!(planning.text.contains(&hash_bytes(file.as_bytes())));
        let inventory: Vec<serde_json::Value> = serde_json::from_str(&file).unwrap();
        assert_eq!(inventory.len(), 503, "500 pages + index + log + existing page");
        let log = inventory.iter().find(|entry| entry["path"] == "wiki/log.md").unwrap();
        assert_eq!(log["planningUse"], "retained_history_local_append_only");
        assert!(!file.contains("small history"), "the inventory carries metadata, never the log body");
        // 页面少到可以全量内联时，旧清单文件会被清掉，避免模型读到过期清单。
        for index in 10..500 {
            std::fs::remove_file(w.join(format!("wiki/concepts/页面-{index:04}.md"))).unwrap();
        }
        assert_eq!(prompt_with_budget(narrow(), w, "zh-CN", POLICY).unwrap().mode, PlanningMode::FullInline);
        assert!(!w.join(WORKSPACE_WIKI_INVENTORY).exists());
    }

    #[test]
    fn validate_input_applies_each_agents_budget() {
        // Claude 现在也做预检：默认硬上限 500,000 token = 1,000,000 字节。
        validate_input(AgentKind::Claude, &"a".repeat(1_000_000)).unwrap();
        assert_eq!(validate_input(AgentKind::Claude, &"a".repeat(1_000_001)).unwrap_err().code, "COMPILE_AGENT_INPUT_TOO_LARGE");
        // Codex 的 token 预算（默认 250,000 token）和 CLI 字符上限都要检查。
        let error = validate_input(AgentKind::Codex, &"中".repeat(200_000)).unwrap_err();
        assert_eq!(error.code, "COMPILE_AGENT_INPUT_TOO_LARGE");
        assert!(!format!("{error:?}").contains("中中中"), "errors must contain counts, not private prompt bodies");
        // 未验证窗口的 Agent 保持 V7 行为，不做预检。
        validate_input(AgentKind::Hermes, &"a".repeat(3_000_000)).unwrap();
    }

    #[test]
    fn planning_reads_reject_traversal_and_external_symlinks() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_input(root.path(), "../outside.md", true).unwrap_err().code, "COMPILE_INPUT_PATH_INVALID");
        assert_eq!(read_input(root.path(), "/outside.md", true).unwrap_err().code, "COMPILE_INPUT_PATH_INVALID");
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("private.md"), "OUTSIDE_PRIVATE_CONTENT").unwrap();
            std::os::unix::fs::symlink(outside.path().join("private.md"), root.path().join("linked.md")).unwrap();
            let error = read_input(root.path(), "linked.md", true).unwrap_err();
            assert_eq!(error.code, "COMPILE_INPUT_PATH_INVALID");
            assert!(!format!("{error:?}").contains("OUTSIDE_PRIVATE_CONTENT"));
        }
    }
}
