//! Planning context for a read-only Agent; complete files remain in its workspace.
use super::*;

pub(super) const CODEX_MAX_INPUT_CHARACTERS: usize = 1_048_576;

pub(super) fn prompt(
    agent: AgentKind,
    workspace: &Path,
    language: &str,
    policy: CompileGenerationPolicy,
) -> Result<String, BackendError> {
    let full = CompileService::provider_plan_prompt_with_policy(workspace, language, policy)?;
    if agent != AgentKind::Codex || character_units(&full) <= CODEX_MAX_INPUT_CHARACTERS {
        return Ok(full);
    }
    // Codex's read-only workspace tools can retrieve exact existing pages.
    // Other Agent profiles may have tools disabled and must not use this route.
    let catalog_path = workspace.join(WORKSPACE_SOURCE_INPUTS);
    if !catalog_path.is_file() {
        validate_input(agent, &full)?;
        return Ok(full);
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
    for source in &catalog {
        if seen.insert(source.path.clone()) {
            let content = read_input(workspace, &source.path, true)?;
            prompt.push_str(&format!("\n--- {} ---\n{content}", source.path));
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
            "planningUse": if path == "wiki/log.md" { "retained_history_local_append_only" } else { "read_exact_file_as_needed" }
        }));
    }
    prompt.push_str("\n--- Existing Wiki file inventory; bodies omitted, not deleted or rewritten ---\n");
    prompt.push_str(&serde_json::to_string_pretty(&inventory).map_err(|error| {
        BackendError::new("COMPILE_INPUT_READ_FAILED", error.to_string(), true, false)
    })?);
    validate_input(agent, &prompt)?;
    Ok(prompt)
}

// UTF-16 is conservative for non-BMP characters while matching the observed
// CLI character count for ASCII and CJK; never treat UTF-8 bytes as characters.
fn character_units(prompt: &str) -> usize {
    prompt.encode_utf16().count()
}

pub(super) fn validate_input(agent: AgentKind, prompt: &str) -> Result<(), BackendError> {
    if agent != AgentKind::Codex {
        return Ok(());
    }
    let count = character_units(prompt);
    if count <= CODEX_MAX_INPUT_CHARACTERS {
        return Ok(());
    }
    Err(BackendError::new(
        "COMPILE_AGENT_INPUT_TOO_LARGE",
        "The required Agent input exceeds the CLI character limit. Complete Sources and required page bodies were preserved; no Agent process was launched for this input.",
        true,
        true,
    ).with_details(serde_json::json!({
        "inputCharacterUnits": count,
        "maximumCharacterUnits": CODEX_MAX_INPUT_CHARACTERS,
        "unit": "utf16_conservative",
        "agent": "codex",
    })))
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

    #[test]
    fn codex_large_history_planning_keeps_sources_and_bounds_initial_input() {
        let root = tempfile::tempdir().unwrap();
        let w = root.path();
        std::fs::create_dir_all(w.join("wiki")).unwrap();
        std::fs::create_dir_all(w.join("raw/sources/中文 资料")).unwrap();
        std::fs::write(w.join("purpose.md"), "# Purpose\nPreserve knowledge").unwrap();
        std::fs::write(w.join("schema.md"), "# Schema\nExact source citations").unwrap();
        let path = "raw/sources/中文 资料/原文.md";
        let original = "# 来源\n完整事实和引用 ORIGINAL_SOURCE_END";
        std::fs::write(w.join(path), original).unwrap();
        let catalog = vec![WorkspaceSourceInput {
            registry_source_id: "source-1".into(),
            version_id: "version-1".into(),
            content_hash: hash_bytes(original.as_bytes()),
            path: path.into(),
        }];
        std::fs::write(w.join(WORKSPACE_SOURCE_INPUTS), serde_json::to_vec(&catalog).unwrap()).unwrap();
        let history = "HISTORICAL_LOG_RECORD\n".repeat(60_000);
        std::fs::write(w.join("wiki/log.md"), &history).unwrap();
        std::fs::write(w.join("wiki/index.md"), "# Index\n[[中文 页面]]").unwrap();
        std::fs::write(w.join("wiki/中文 页面.md"), "# Existing\nUSER_EDIT_MUST_SURVIVE").unwrap();
        let full = CompileService::provider_plan_prompt_with_policy(w, "zh-CN", CompileGenerationPolicy::WorkflowReviewableDeletes).unwrap();
        assert!(full.chars().count() > CODEX_MAX_INPUT_CHARACTERS, "fixture must reproduce the actual oversized planning input");
        assert_eq!(prompt(AgentKind::Claude, w, "zh-CN", CompileGenerationPolicy::WorkflowReviewableDeletes).unwrap(), full, "tool-disabled profiles must not receive a file-reading fallback");
        let bounded = prompt(AgentKind::Codex, w, "zh-CN", CompileGenerationPolicy::WorkflowReviewableDeletes).unwrap();
        let trusted_header = bounded.split("--- purpose.md ---").next().unwrap();
        assert!(!trusted_header.contains("you have no filesystem or tool access"), "bounded Agent planning must not inherit the tool-disabled BYOK contract");
        assert!(trusted_header.contains("Use read-only file tools"), "bounded Agent planning must explicitly allow evidence reads");
        assert!(trusted_header.contains("Never write, modify, or delete files"), "planning must remain read-only");
        assert!(bounded.chars().count() < 128_000, "large history should be inventoried, not sent in full");
        assert!(bounded.contains(original));
        assert!(bounded.contains(path));
        assert!(bounded.contains("source-1") && bounded.contains("version-1"));
        assert!(bounded.contains("wiki/中文 页面.md"));
        assert!(bounded.contains(&hash_bytes(history.as_bytes())));
        assert!(!bounded.contains("HISTORICAL_LOG_RECORD"));
        assert_eq!(std::fs::read_to_string(w.join("wiki/log.md")).unwrap(), history);
        assert_eq!(std::fs::read_to_string(w.join(path)).unwrap(), original);
        assert_eq!(std::fs::read_to_string(w.join("wiki/中文 页面.md")).unwrap(), "# Existing\nUSER_EDIT_MUST_SURVIVE");

        let oversized_source = format!("{}REQUIRED_SOURCE_END", "选".repeat(CODEX_MAX_INPUT_CHARACTERS));
        std::fs::write(w.join(path), &oversized_source).unwrap();
        let error = prompt(AgentKind::Codex, w, "zh-CN", CompileGenerationPolicy::WorkflowReviewableDeletes).unwrap_err();
        assert_eq!(error.code, "COMPILE_AGENT_INPUT_TOO_LARGE");
        assert_eq!(std::fs::read_to_string(w.join(path)).unwrap(), oversized_source, "required Source must never be truncated to fit");
    }

    #[test]
    fn small_planning_prompts_remain_identical() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("wiki")).unwrap();
        std::fs::write(root.path().join("purpose.md"), "Purpose").unwrap();
        std::fs::write(root.path().join("schema.md"), "Schema").unwrap();
        std::fs::write(root.path().join("wiki/log.md"), "History").unwrap();
        let expected = CompileService::provider_plan_prompt_with_policy(root.path(), "en", CompileGenerationPolicy::WorkflowReviewableDeletes).unwrap();
        assert_eq!(prompt(AgentKind::Codex, root.path(), "en", CompileGenerationPolicy::WorkflowReviewableDeletes).unwrap(), expected);
    }

    #[test]
    fn invocation_budget_counts_characters_and_checks_exact_boundary() {
        let cjk = "中".repeat(CODEX_MAX_INPUT_CHARACTERS);
        assert!(cjk.len() > CODEX_MAX_INPUT_CHARACTERS);
        validate_input(AgentKind::Codex, &cjk).unwrap();
        let error = validate_input(AgentKind::Codex, &(cjk + "多")).unwrap_err();
        assert_eq!(error.code, "COMPILE_AGENT_INPUT_TOO_LARGE");
        assert!(!format!("{error:?}").contains("中中中"), "errors must contain counts, not private prompt bodies");
        assert!(validate_input(AgentKind::Codex, &"😀".repeat(CODEX_MAX_INPUT_CHARACTERS / 2 + 1)).is_err());
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
