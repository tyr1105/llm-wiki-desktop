//! Agent 输入预算（V8）。
//!
//! 做什么：按 Agent 给出"上下文窗口"和两个阈值——
//!   * 内联阈值（窗口的 20%）：全量提示词不超过它，就照旧整篇内嵌；超过则改走
//!     "来源全文 + 页面清单、按需读文件"的有界模式（前提是该 Agent 有只读文件工具）。
//!   * 单次初始输入硬上限（窗口的 50%）：任何一次调用的初始输入超过它，就在启动
//!     命令行之前报 COMPILE_AGENT_INPUT_TOO_LARGE，不浪费一次调用；剩下的一半窗口
//!     留给模型按需读文件和输出。
//!
//! 为什么：V7 只给 Codex 设了 1,048,576 个 UTF-16 字符的命令行硬上限，Claude 完全
//! 没有预检，只能等 CLI 报 "Prompt is too long" 后秒败。V8 把"什么时候切换、单次
//! 最多给多少"统一改成按 token 预算判断。
//!
//! token 估算一律用"UTF-8 字节数 ÷ 2"（向上取整）：实测 3.04MB 输入约 139 万 token
//! （约 0.45 token/字节），÷2 大约多估 11%，偏保守；中文为主的提示词会被明显高估，
//! 宁可提前切换到有界模式，也不让 CLI 在本地拦截。
use super::*;

/// 覆盖默认窗口的环境变量，格式如 `claude=1000000,codex=500000`。
/// 由启动后端的脚本传给后端进程本身（后端会清空 Agent 子进程的环境变量，
/// 所以它只影响这里的预算计算，不会传给 claude/codex 命令行）。
pub(super) const CONTEXT_TOKENS_ENV: &str = "LLM_WIKI_AGENT_CONTEXT_TOKENS";

/// Codex 命令行对单次输入的硬上限（UTF-16 字符数），沿用 V7 的实测值。
/// V8 对 Codex 同时检查这个字符上限和 token 预算，两个都必须满足。
pub(super) const CODEX_MAX_INPUT_CHARACTERS: usize = 1_048_576;

/// Claude 默认窗口：取 `opus` 别名的实测值 1,000,000 tokens。
///
/// 依据（设计说明"必须采纳的修正"：在逐个模型探测之前，Claude 的预算按 opus 别名的
/// 实测值来设）：模型设为 `opus` 别名时，CLI 2.1.259
/// 把它解析为 claude-opus-5，真实调用返回的上限是 `limit 1000000`。
///
/// 为什么不再用 200,000 的保守值（评审确认的问题）：本知识库每批都会更新的 index.md
/// 有约 20.5 万字节（10 行超过 2000 字符，最长 19,821 字符），按 20 万窗口算硬上限只有
/// 10 万 token ≈ 20 万字节，目标页基线放不进提示，只能退到整体文件模式，PageEdits 的
/// 锚点要靠模型用 Read 读回来——而 Read 会不会截断长行还没实测。按实测窗口设预算，
/// 基线就能一直内联在提示里。
///
/// 这不是"所有 Claude 模型都是 1M"：后端看不到包装脚本实际用哪个模型。如果改用 CLI
/// 不认识的模型 ID（例如 claude-*-5-5，实测会按更小的假定窗口在本地 blocking_limit），
/// 失败会被改动集 D 归类为 context_limit、明确报出来；那时应在启动后端时用
/// LLM_WIKI_AGENT_CONTEXT_TOKENS（例如 `claude=200000`）把窗口调小。
const CLAUDE_DEFAULT_CONTEXT_TOKENS: u64 = 1_000_000;

/// Codex 默认窗口：500,000 tokens（来自调研背景信息，尚未实测，可用环境变量覆盖）。
const CODEX_DEFAULT_CONTEXT_TOKENS: u64 = 500_000;

/// 内联阈值占窗口的百分比。
const INLINE_PERCENT: u64 = 20;
/// 单次初始输入硬上限占窗口的百分比。
const HARD_PERCENT: u64 = 50;

/// 环境变量里允许的窗口范围；超出范围的值视为写错，忽略并回退默认值，
/// 防止误写成 `claude=1000`（会让每次整理都报超限）或多写几个 0。
const MIN_OVERRIDE_CONTEXT_TOKENS: u64 = 8_000;
const MAX_OVERRIDE_CONTEXT_TOKENS: u64 = 10_000_000;

/// 窗口数值的来源，写进错误详情和任务日志，方便判断是不是没配上环境变量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WindowSource {
    /// 使用代码里的保守默认值。
    Default,
    /// 来自 LLM_WIKI_AGENT_CONTEXT_TOKENS。
    EnvOverride,
    /// 没有经过验证的窗口（Openclaw/Hermes），保持 V7 行为：不做 token 预检。
    Unbounded,
}

impl WindowSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::EnvOverride => "env_override",
            Self::Unbounded => "unbounded",
        }
    }
}

/// 超限的原因，写进错误详情的 `reason` 字段。重试脚本据此决定要不要拆分组：
/// 只有 `selected_sources`（所选来源本身太大）时拆组才有用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OversizeReason {
    /// 所选来源全文本身就放不下；去掉来源后其余部分能放下，拆分组可以解决。
    SelectedSources,
    /// 固定部分（提示头、purpose/schema、页面清单）就已超限，拆分组也没用。
    FixedContext,
    /// 不走有界模式的 Agent 拿到的全量工作区提示超限。
    FullWorkspace,
    /// 页面生成等其他必需输入超限（目标页基线、本组来源等不能截断的内容）。
    RequiredInput,
}

impl OversizeReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::SelectedSources => "selected_sources",
            Self::FixedContext => "fixed_context",
            Self::FullWorkspace => "full_workspace",
            Self::RequiredInput => "required_input",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::SelectedSources => "The selected Sources alone exceed this Agent's input budget. Select fewer Sources or split this batch. Complete Sources were preserved; no Agent process was launched for this input.",
            Self::FixedContext => "The fixed planning context (instructions, purpose, schema and page inventory) exceeds this Agent's input budget; splitting the Source batch will not help. No Agent process was launched for this input.",
            Self::FullWorkspace => "The complete embedded workspace exceeds this Agent's input budget. Complete Sources and pages were preserved; no Agent process was launched for this input.",
            Self::RequiredInput => "The required Agent input exceeds this Agent's input budget. Complete Sources and required page bodies were preserved; no Agent process was launched for this input.",
        }
    }
}

/// 某个 Agent 的输入预算。所有 token 数都按"UTF-8 字节数 ÷ 2"估算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AgentInputBudget {
    pub(super) agent: AgentKind,
    /// 上下文窗口（tokens）；None 表示没有经过验证的窗口，不做 token 预检。
    pub(super) context_tokens: Option<u64>,
    /// 内联阈值（tokens）：全量提示不超过它就整篇内嵌。
    pub(super) inline_tokens: Option<u64>,
    /// 单次初始输入硬上限（tokens）。
    pub(super) hard_tokens: Option<u64>,
    /// 命令行字符硬上限（UTF-16 单位），目前只有 Codex 有。
    pub(super) cli_character_units: Option<usize>,
    /// 该 Agent 的整理调用是否具备只读文件工具（Read/Grep/Glob 或只读沙箱）。
    /// 只有为 true 时才允许走"只给清单、让模型自己读文件"的有界模式。
    pub(super) read_only_tools: bool,
    pub(super) window_source: WindowSource,
}

/// 估算 token 数：UTF-8 字节数 ÷ 2，向上取整。
pub(super) fn estimate_tokens(text: &str) -> u64 {
    tokens_for_bytes(text.len())
}

/// 同上，但直接接收字节数，用于估算"去掉某部分后"的假设尺寸。
pub(super) fn tokens_for_bytes(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(2)
}

/// 命令行字符数口径：UTF-16 单位。对 ASCII 和中文与 CLI 实测计数一致，
/// 对表情等非 BMP 字符偏保守；绝不能把 UTF-8 字节数当成字符数。
pub(super) fn character_units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 该 Agent 的整理调用是否具备只读文件工具。
///
/// Codex：`codex exec --sandbox read-only`，V7 起就是这样。
/// Claude：V8 改动集 B 新增的只读整理配置（--tools=Read,Grep,Glob + --restricted
/// + --safe-mode + 沙箱）。规划/逐页生成在走有界模式时必须用那套配置调用 Claude，
/// 聊天用的 chat_invocation 不变。这个函数就是两边对齐的接口：返回 true 的 Agent
/// 才会拿到"按需读文件"的有界提示词，调用方据此选择带只读工具的调用方式。
/// Openclaw/Hermes：没有经过验证的只读配置，保持 V7 的全量内联行为。
pub(super) fn has_read_only_compile_tools(agent: AgentKind) -> bool {
    matches!(agent, AgentKind::Codex | AgentKind::Claude)
}

/// 解析 LLM_WIKI_AGENT_CONTEXT_TOKENS，返回指定 Agent 的覆盖值。
///
/// 规则：逗号分隔的 `agent=tokens` 列表；agent 名不区分大小写（claude/codex/
/// openclaw/hermes）；数字里允许下划线（1_000_000）；同一 Agent 写多次以最后一个
/// 合法值为准。写错的条目（名字不认识、不是数字、超出合理范围）直接忽略，
/// 回退到默认值——错误详情和任务日志里的 windowSource 会显示 default，便于发现。
pub(super) fn context_override_for(agent: AgentKind, raw: &str) -> Option<u64> {
    let mut found = None;
    for entry in raw.split(',') {
        let Some((name, value)) = entry.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case(agent.command()) {
            continue;
        }
        let digits = value.trim().replace('_', "");
        let Ok(tokens) = digits.parse::<u64>() else {
            continue;
        };
        if (MIN_OVERRIDE_CONTEXT_TOKENS..=MAX_OVERRIDE_CONTEXT_TOKENS).contains(&tokens) {
            found = Some(tokens);
        }
    }
    found
}

impl AgentInputBudget {
    /// 生产入口：默认值 + 后端进程环境变量 LLM_WIKI_AGENT_CONTEXT_TOKENS 的覆盖。
    /// 每次调用都重新读取环境变量，代价可以忽略。
    pub(super) fn for_agent(agent: AgentKind) -> Self {
        Self::from_env_var(agent, CONTEXT_TOKENS_ENV)
    }

    /// 从指定名字的环境变量读取覆盖值。单独拆出来是为了测试能用一个
    /// 独占的变量名验证"环境变量覆盖生效"，避免并行测试互相改同一个变量。
    pub(super) fn from_env_var(agent: AgentKind, name: &str) -> Self {
        Self::for_agent_with_overrides(agent, std::env::var(name).ok().as_deref())
    }

    /// 纯函数版本：给定覆盖字符串（可以没有），算出该 Agent 的预算。
    pub(super) fn for_agent_with_overrides(agent: AgentKind, overrides: Option<&str>) -> Self {
        let default = match agent {
            AgentKind::Claude => Some(CLAUDE_DEFAULT_CONTEXT_TOKENS),
            AgentKind::Codex => Some(CODEX_DEFAULT_CONTEXT_TOKENS),
            // 没有经过验证的窗口：保持 V7 行为，不做 token 预检；
            // 如果用户确实知道窗口，也可以用环境变量为它们开启预算。
            AgentKind::Openclaw | AgentKind::Hermes => None,
        };
        let (context_tokens, window_source) =
            match overrides.and_then(|raw| context_override_for(agent, raw)) {
                Some(tokens) => (Some(tokens), WindowSource::EnvOverride),
                None => (
                    default,
                    if default.is_some() {
                        WindowSource::Default
                    } else {
                        WindowSource::Unbounded
                    },
                ),
            };
        Self {
            agent,
            context_tokens,
            inline_tokens: context_tokens.map(|tokens| tokens * INLINE_PERCENT / 100),
            hard_tokens: context_tokens.map(|tokens| tokens * HARD_PERCENT / 100),
            // Codex 的命令行字符上限与 token 预算是两回事，两个都要检查。
            cli_character_units: (agent == AgentKind::Codex).then_some(CODEX_MAX_INPUT_CHARACTERS),
            read_only_tools: has_read_only_compile_tools(agent),
            window_source,
        }
    }

    /// 全量提示能否直接内联：token 不超过内联阈值，且（如有）不超过命令行字符上限。
    /// 没有窗口的 Agent 永远"能内联"，即保持 V7 的全量行为。
    pub(super) fn fits_inline(&self, text: &str) -> bool {
        let tokens_ok = self
            .inline_tokens
            .is_none_or(|limit| estimate_tokens(text) <= limit);
        let characters_ok = self
            .cli_character_units
            .is_none_or(|limit| character_units(text) <= limit);
        tokens_ok && characters_ok
    }

    /// 按"字节数 + UTF-16 字符数"判断超出了哪些硬上限；空列表表示没超。
    /// 单独接收计数而不是字符串，是为了能估算"去掉来源后还剩多少"这类假设尺寸。
    pub(super) fn exceeded_limits(&self, bytes: usize, units: usize) -> Vec<&'static str> {
        let mut exceeded = Vec::new();
        if self
            .hard_tokens
            .is_some_and(|limit| tokens_for_bytes(bytes) > limit)
        {
            exceeded.push("context_token_budget");
        }
        if self.cli_character_units.is_some_and(|limit| units > limit) {
            exceeded.push("cli_character_limit");
        }
        exceeded
    }

    /// 校验单次初始输入；超限时原因记为 required_input（页面生成等通用场景）。
    pub(super) fn validate(&self, text: &str) -> Result<(), BackendError> {
        self.validate_with_reason(text, OversizeReason::RequiredInput, serde_json::json!({}))
    }

    /// 校验单次初始输入，并在超限时写明原因和额外的诊断字段（只含计数，不含正文）。
    pub(super) fn validate_with_reason(
        &self,
        text: &str,
        reason: OversizeReason,
        extra: serde_json::Value,
    ) -> Result<(), BackendError> {
        let bytes = text.len();
        let units = character_units(text);
        let exceeded = self.exceeded_limits(bytes, units);
        if exceeded.is_empty() {
            return Ok(());
        }
        Err(self.too_large(bytes, units, exceeded, reason, extra))
    }

    /// 生成 COMPILE_AGENT_INPUT_TOO_LARGE 错误。详情里只放计数和配置，
    /// 绝不放提示词正文（可能含私人资料）。
    fn too_large(
        &self,
        bytes: usize,
        units: usize,
        exceeded: Vec<&'static str>,
        reason: OversizeReason,
        extra: serde_json::Value,
    ) -> BackendError {
        let mut details = serde_json::json!({
            "agent": self.agent.command(),
            "reason": reason.as_str(),
            "exceededLimits": exceeded,
            "estimatedTokens": tokens_for_bytes(bytes),
            "inputBytes": bytes,
            "hardTokens": self.hard_tokens,
            "inlineTokens": self.inline_tokens,
            "contextTokens": self.context_tokens,
            "tokenUnit": "utf8_bytes_div_2",
            "windowSource": self.window_source.as_str(),
            // 以下三个键沿用 V7 的命名，旧脚本读到的含义不变。
            "inputCharacterUnits": units,
            "maximumCharacterUnits": self.cli_character_units,
            "unit": "utf16_conservative",
        });
        if let (Some(target), serde_json::Value::Object(fields)) = (details.as_object_mut(), extra) {
            target.extend(fields);
        }
        BackendError::new("COMPILE_AGENT_INPUT_TOO_LARGE", reason.message(), true, true)
            .with_details(details)
    }

    /// 一行人能读懂的预算说明，写进任务日志。
    pub(super) fn describe(&self) -> String {
        match (self.context_tokens, self.inline_tokens, self.hard_tokens) {
            (Some(context), Some(inline), Some(hard)) => format!(
                "inline threshold {inline} / hard limit {hard} of a {context}-token window ({}){}",
                self.window_source.as_str(),
                // Claude 用默认窗口时提醒一句它的来历：这是 opus 别名的实测值，换了模型
                // 就要在启动后端时用环境变量改，否则 CLI 可能在本地按更小的窗口拦截。
                if self.agent == AgentKind::Claude && self.window_source == WindowSource::Default {
                    format!("; default is the measured window of the `opus` alias, set {CONTEXT_TOKENS_ENV} for other models")
                } else {
                    String::new()
                }
            ),
            _ => "no verified token window (unbounded, V7 behavior)".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_measured_windows_and_codex_keeps_its_cli_limit() {
        // Claude 默认按 opus 别名的实测窗口 1,000,000 算；阈值是 20% 和 50%。
        let claude = AgentInputBudget::for_agent_with_overrides(AgentKind::Claude, None);
        assert_eq!(claude.context_tokens, Some(1_000_000));
        assert_eq!(claude.inline_tokens, Some(200_000));
        assert_eq!(claude.hard_tokens, Some(500_000));
        assert_eq!(claude.cli_character_units, None);
        assert!(claude.read_only_tools);
        assert_eq!(claude.window_source, WindowSource::Default);
        // 任务日志里写明默认值的来历，并提示换模型时用哪个环境变量改。
        assert!(claude.describe().contains("measured window of the `opus` alias"));
        assert!(claude.describe().contains(CONTEXT_TOKENS_ENV));
        // 评审确认的场景：真实 index.md 约 20.5 万字节（含长行），整页加上计划和来源
        // 也远在默认硬上限（100 万字节）以内，目标页基线可以一直内联。
        assert!(claude.validate(&"索".repeat(205_000)).is_ok());
        // Codex 同时保留 1,048,576 字符的 CLI 硬上限。
        let codex = AgentInputBudget::for_agent_with_overrides(AgentKind::Codex, None);
        assert_eq!(codex.context_tokens, Some(500_000));
        assert_eq!(codex.cli_character_units, Some(CODEX_MAX_INPUT_CHARACTERS));
        assert!(codex.read_only_tools);
        // 没有验证过只读配置的 Agent 不走有界模式，也不做 token 预检（V7 行为）。
        for agent in [AgentKind::Openclaw, AgentKind::Hermes] {
            let budget = AgentInputBudget::for_agent_with_overrides(agent, None);
            assert!(!budget.read_only_tools);
            assert_eq!(budget.window_source, WindowSource::Unbounded);
            assert!(budget.fits_inline(&"x".repeat(5_000_000)));
            budget.validate(&"x".repeat(5_000_000)).unwrap();
        }
    }

    #[test]
    fn token_estimate_is_utf8_bytes_div_two_and_checks_exact_boundary() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("ab"), 1);
        // 一个汉字 3 字节，按 ÷2 估算为 2 token，偏保守。
        assert_eq!(estimate_tokens("中"), 2);
        // 用 20 万窗口（例如 CLI 不认识、按较小窗口处理的模型）验证边界换算。
        let claude = AgentInputBudget::for_agent_with_overrides(AgentKind::Claude, Some("claude=200000"));
        // 200,000 字节 = 100,000 token，正好等于硬上限，允许。
        claude.validate(&"a".repeat(200_000)).unwrap();
        let error = claude.validate(&"a".repeat(200_001)).unwrap_err();
        assert_eq!(error.code, "COMPILE_AGENT_INPUT_TOO_LARGE");
        let details = error.details.clone().unwrap();
        assert_eq!(details["reason"], "required_input");
        assert_eq!(details["estimatedTokens"], 100_001);
        assert_eq!(details["hardTokens"], 100_000);
        assert_eq!(details["tokenUnit"], "utf8_bytes_div_2");
        assert_eq!(details["exceededLimits"][0], "context_token_budget");
        assert_eq!(details["windowSource"], "env_override");
        assert!(!format!("{error:?}").contains("aaaa"), "errors carry counts, never prompt bodies");
        // 内联阈值：80,000 字节 = 40,000 token 以内整篇内嵌，多 1 个 token 就切换。
        assert!(claude.fits_inline(&"a".repeat(80_000)));
        assert!(!claude.fits_inline(&"a".repeat(80_001)));
        // 默认窗口（100 万）下的同一组边界：硬上限 50 万 token = 100 万字节。
        let default = AgentInputBudget::for_agent_with_overrides(AgentKind::Claude, None);
        default.validate(&"a".repeat(1_000_000)).unwrap();
        assert_eq!(default.validate(&"a".repeat(1_000_001)).unwrap_err().details.unwrap()["hardTokens"], 500_000);
        assert!(default.fits_inline(&"a".repeat(400_000)));
        assert!(!default.fits_inline(&"a".repeat(400_001)));
    }

    #[test]
    fn codex_checks_both_token_budget_and_cli_characters() {
        // 窗口调到很大时，只剩字符上限在起作用：正好 1,048,576 个汉字允许，多一个就超。
        let wide = AgentInputBudget::for_agent_with_overrides(AgentKind::Codex, Some("codex=10000000"));
        let cjk = "中".repeat(CODEX_MAX_INPUT_CHARACTERS);
        wide.validate(&cjk).unwrap();
        let error = wide.validate(&(cjk.clone() + "多")).unwrap_err();
        assert_eq!(error.details.as_ref().unwrap()["exceededLimits"], serde_json::json!(["cli_character_limit"]));
        assert!(wide.validate(&"😀".repeat(CODEX_MAX_INPUT_CHARACTERS / 2 + 1)).is_err());
        // 默认窗口下，同一段文本（约 157 万 token）两个上限都超。
        let default = AgentInputBudget::for_agent_with_overrides(AgentKind::Codex, None);
        let error = default.validate(&(cjk + "多")).unwrap_err();
        assert_eq!(
            error.details.as_ref().unwrap()["exceededLimits"],
            serde_json::json!(["context_token_budget", "cli_character_limit"])
        );
        // 字符数没超、token 超了，也必须拦下：只看字符上限是 V7 的老问题。
        let error = default.validate(&"中".repeat(200_000)).unwrap_err();
        assert_eq!(error.details.as_ref().unwrap()["exceededLimits"], serde_json::json!(["context_token_budget"]));
    }

    #[test]
    fn environment_override_parses_per_agent_and_ignores_invalid_entries() {
        let raw = " Claude = 1_000_000 , codex=500000,hermes=abc,openclaw=10,unknown=900000";
        let claude = AgentInputBudget::for_agent_with_overrides(AgentKind::Claude, Some(raw));
        assert_eq!(claude.context_tokens, Some(1_000_000));
        assert_eq!(claude.inline_tokens, Some(200_000));
        assert_eq!(claude.hard_tokens, Some(500_000));
        assert_eq!(claude.window_source, WindowSource::EnvOverride);
        let codex = AgentInputBudget::for_agent_with_overrides(AgentKind::Codex, Some(raw));
        assert_eq!(codex.context_tokens, Some(500_000));
        assert_eq!(codex.cli_character_units, Some(CODEX_MAX_INPUT_CHARACTERS), "override never drops the CLI limit");
        // 写错的值（非数字、低于下限）被忽略，回退默认值。
        let hermes = AgentInputBudget::for_agent_with_overrides(AgentKind::Hermes, Some(raw));
        assert_eq!(hermes.window_source, WindowSource::Unbounded);
        let openclaw = AgentInputBudget::for_agent_with_overrides(AgentKind::Openclaw, Some(raw));
        assert_eq!(openclaw.context_tokens, None);
        let typo = AgentInputBudget::for_agent_with_overrides(AgentKind::Claude, Some("claude=1000,claude=abc"));
        assert_eq!(typo.context_tokens, Some(1_000_000));
        assert_eq!(typo.window_source, WindowSource::Default);
        // 环境变量也能把 Claude 窗口调小（换成 CLI 按较小窗口处理的模型时）。
        let smaller = AgentInputBudget::for_agent_with_overrides(AgentKind::Claude, Some("claude=200000"));
        assert_eq!(smaller.hard_tokens, Some(100_000));
        assert!(!smaller.describe().contains("opus"), "the opus hint only accompanies the default window");
        // 同一 Agent 写多次，以最后一个合法值为准。
        assert_eq!(context_override_for(AgentKind::Claude, "claude=300000,claude=400000"), Some(400_000));
        // 未经验证的 Agent 也可以由用户显式开启预算。
        let enabled = AgentInputBudget::for_agent_with_overrides(AgentKind::Hermes, Some("hermes=100000"));
        assert_eq!(enabled.hard_tokens, Some(50_000));
        assert!(!enabled.read_only_tools, "a window override never grants file tools");
    }

    #[test]
    fn environment_variable_override_takes_effect() {
        // 用独占的变量名，避免与并行测试读写真实变量名互相干扰；
        // 生产入口 for_agent 读的是 CONTEXT_TOKENS_ENV，走同一段代码。
        let name = format!("LLM_WIKI_AGENT_CONTEXT_TOKENS_TEST_{}", uuid::Uuid::new_v4().simple());
        assert_eq!(AgentInputBudget::from_env_var(AgentKind::Claude, &name).context_tokens, Some(1_000_000));
        std::env::set_var(&name, "claude=300000,codex=400000");
        let claude = AgentInputBudget::from_env_var(AgentKind::Claude, &name);
        let codex = AgentInputBudget::from_env_var(AgentKind::Codex, &name);
        std::env::remove_var(&name);
        assert_eq!(claude.context_tokens, Some(300_000));
        assert_eq!(claude.window_source, WindowSource::EnvOverride);
        assert_eq!(codex.context_tokens, Some(400_000));
        assert!(claude.describe().contains("env_override"));
    }
}
