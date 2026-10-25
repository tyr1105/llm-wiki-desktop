use serde::{Deserialize, Serialize};

use super::{agent::AgentKind, llm::LlmProviderKind};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompileRoutePreference {
    Auto,
    Agent,
    Byok,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompileRoute {
    Agent,
    Byok,
}

/// A concrete, already-resolved execution route. New Workflows callers must
/// pass one of these and may not ask Compile to silently fall back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ResolvedCompileRoute {
    Agent {
        agent: AgentKind,
        model: Option<String>,
    },
    Byok {
        provider: LlmProviderKind,
        model: String,
    },
}

impl ResolvedCompileRoute {
    pub fn legacy_kind(&self) -> CompileRoute {
        match self {
            Self::Agent { .. } => CompileRoute::Agent,
            Self::Byok { .. } => CompileRoute::Byok,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompileConflictResolution {
    KeepCurrent,
    UseGenerated,
    ManualMerge,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompileFile {
    pub path: String,
    pub content: String,
}

impl CompileFile {
    pub fn new(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompileManifest {
    pub files: Vec<CompileFile>,
    #[serde(default)]
    pub deletions: Vec<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompileCandidate {
    pub route: ResolvedCompileRoute,
    pub plan: CompilePlan,
    pub manifest: CompileManifest,
    #[serde(default)]
    pub source_outcomes: Vec<CompileSourceOutcome>,
    /// Backend-bound historical evidence. It never expands the consumption scope.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_dependencies: Vec<CompileSourceDependency>,
    /// V8：本批候选的完整审计记录（V7 时整段 JSON 追加在 wiki/log.md 里）。
    ///
    /// 做什么：随候选一起持久化（候选文件有权威哈希，审阅期间改不了），只有审阅通过、
    /// 应用成功时才和 CompileRecord 在同一个文件事务里写到 `.app/.../compile-audit/`；
    /// 审阅不通过、候选被丢弃时它也随之消失，不会单独生效。
    ///
    /// 为什么：审计 JSON 里约 79% 是旧来源依赖清单，每批让 log.md 涨约 1 万字符；
    /// 而 CompileRecord 只有 7 个字段，不含这些信息，不能直接丢。旧候选没有这个字段
    /// （默认 None），照旧可以读取和应用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_record: Option<CompileCandidateAudit>,
}

/// V8：候选审计文件——写到哪里、写什么（逐字节）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompileCandidateAudit {
    /// 项目内相对路径（已按项目布局映射），例如 `.app/compat/compile-audit/<任务编号>.json`。
    /// 放在 wiki/ 和 raw/extracted 之外：编译只扫描这两处，审计文件永远不会被
    /// 当成页面或来源拉进下一次整理的上下文。
    pub path: String,
    /// 要写入的完整 JSON 文本。保存成文本而不是再序列化一次，保证审阅时看到的、
    /// 检查点里记下的和最终落盘的是同一串字节。
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompileSourceDependency {
    pub source_version: SourceVersionRef,
    pub project_path: String,
    pub workspace_path: String,
    pub file_hash: String,
}

impl CompileCandidate {
    /// Only accepted outcomes enter the receipt and its metadata write set.
    pub fn consumed_source_versions(&self) -> Vec<SourceVersionRef> {
        self.source_outcomes
            .iter()
            .filter(|outcome| outcome.status != CompileSourceOutcomeStatus::Deferred)
            .map(|outcome| outcome.source_version.clone())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompileSourceOutcome {
    pub source_version: SourceVersionRef,
    #[serde(default)]
    pub source_path: String,
    #[serde(default)]
    pub status: CompileSourceOutcomeStatus,
    pub integrated_paths: Vec<String>,
    #[serde(default)]
    pub evidence_path: Option<String>,
    #[serde(default)]
    pub evidence_excerpt: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompileSourceOutcomeStatus {
    #[default]
    Integrated,
    AlreadyCovered,
    Deferred,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompilePlanSourceDecision {
    pub source_id: String,
    pub status: CompileSourceOutcomeStatus,
    pub reason: String,
    #[serde(default)]
    pub evidence_path: Option<String>,
    #[serde(default)]
    pub evidence_excerpt: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompileChangeSummary {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub skipped: Vec<String>,
    pub deleted: Vec<String>,
    pub conflicted: Vec<String>,
    pub high_risk: Vec<String>,
}

impl CompileChangeSummary {
    pub fn affected_paths(&self) -> Vec<String> {
        let mut paths = self
            .created
            .iter()
            .chain(self.updated.iter())
            .chain(self.deleted.iter())
            .chain(self.conflicted.iter())
            .cloned()
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
    }

    pub fn requires_confirmation(&self) -> bool {
        !self.deleted.is_empty() || !self.conflicted.is_empty() || !self.high_risk.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompilePlan {
    pub summary: String,
    pub items: Vec<CompilePlanItem>,
    #[serde(default)]
    pub global_risk_flags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_decisions: Vec<CompilePlanSourceDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompilePlanItem {
    pub action: CompileAction,
    pub target_path: String,
    pub page_type: CompilePageType,
    #[serde(default)]
    pub source_ids: Vec<String>,
    #[serde(default)]
    pub affected_existing_pages: Vec<String>,
    pub reason: String,
    #[serde(default)]
    pub risk_flags: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompileAction {
    Create,
    Update,
    Merge,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompilePageType {
    Entity,
    Concept,
    Synthesis,
    Comparison,
    Query,
    Overview,
    Index,
    Log,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceVersionRef {
    pub source_id: String,
    pub version_id: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompileRequest {
    pub project_id: String,
    pub project_root_path: String,
    #[serde(default = "default_route")]
    pub route: CompileRoutePreference,
    pub agent: Option<AgentKind>,
    pub provider: Option<LlmProviderKind>,
    /// Explicit, hash-bound Source versions selected by the user. Paths are
    /// deliberately absent and are resolved only from the trusted registry.
    pub source_versions: Vec<SourceVersionRef>,
}

fn default_route() -> CompileRoutePreference {
    CompileRoutePreference::Auto
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CompileResult {
    pub route: CompileRoute,
    pub affected_paths: Vec<String>,
    pub conflicts: Vec<String>,
    pub checkpoint: Option<String>,
    pub consumed_versions: Vec<SourceVersionRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompileConsumptionRecord {
    pub schema_version: u32,
    pub compile_task_id: String,
    pub route: CompileRoute,
    pub consumed_at: String,
    pub source_versions: Vec<SourceVersionRef>,
    pub affected_paths: Vec<String>,
    pub checkpoint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{CompileAction, CompilePageType, CompilePlan, CompilePlanItem};

    #[test]
    fn compile_plan_serde_uses_camel_case_and_snake_case_enums() {
        let plan = CompilePlan {
            summary: "Create one synthesis and update navigation.".into(),
            items: vec![CompilePlanItem {
                action: CompileAction::Create,
                target_path: "wiki/synthesis/context-engineering.md".into(),
                page_type: CompilePageType::Synthesis,
                source_ids: vec!["wiki/sources/source-a.md".into(), "source-b.md".into()],
                affected_existing_pages: vec!["wiki/index.md".into()],
                reason: "New cross-source theme requires a derived synthesis page.".into(),
                risk_flags: vec!["new_concept".into()],
            }],
            global_risk_flags: vec!["cascade_required".into()],
            source_decisions: Vec::new(),
        };

        let json = serde_json::to_string(&plan).unwrap();

        assert!(json.contains(r#""targetPath":"wiki/synthesis/context-engineering.md""#));
        assert!(json.contains(r#""pageType":"synthesis""#));
        assert!(json.contains(r#""sourceIds":["wiki/sources/source-a.md","source-b.md"]"#));
        assert!(json.contains(r#""affectedExistingPages":["wiki/index.md"]"#));
        assert!(json.contains(r#""globalRiskFlags":["cascade_required"]"#));
        let round_trip: CompilePlan = serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip, plan);
    }
}
