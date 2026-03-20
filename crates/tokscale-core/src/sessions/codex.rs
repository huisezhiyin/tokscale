//! Codex CLI session parser
//!
//! Parses JSONL files from ~/.codex/sessions/
//! Note: This parser has stateful logic to track model and delta calculations.

use super::utils::{
    extract_i64, extract_string, file_modified_timestamp_ms, parse_timestamp_value,
};
use super::UnifiedMessage;
use crate::TokenBreakdown;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;

const STREAMING_WINDOW_MS: i64 = 3_000;
const SETTLING_WINDOW_MS: i64 = 20_000;
const PREPARING_WINDOW_MS: i64 = 5_000;
pub const RECENT_TOKENS_WINDOW_MS: i64 = 60_000;
pub const ACTIVE_SESSION_WINDOW_MS: i64 = 10 * 60 * 1000;

/// Codex entry structure (from JSONL files)
#[derive(Debug, Deserialize)]
pub struct CodexEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub timestamp: Option<String>,
    pub payload: Option<CodexPayload>,
}

#[derive(Debug, Deserialize)]
pub struct CodexPayload {
    #[serde(rename = "type")]
    pub payload_type: Option<String>,
    pub model: Option<String>,
    pub model_name: Option<String>,
    pub cwd: Option<String>,
    pub model_info: Option<CodexModelInfo>,
    pub info: Option<CodexInfo>,
    pub source: Option<String>,
    /// Provider identity from session_meta (e.g. "openai", "azure")
    pub model_provider: Option<String>,
    /// Agent name from session_meta
    pub agent_nickname: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CodexModelInfo {
    pub slug: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CodexInfo {
    pub model: Option<String>,
    pub model_name: Option<String>,
    pub last_token_usage: Option<CodexTokenUsage>,
    pub total_token_usage: Option<CodexTokenUsage>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CodexTokenUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub cache_read_input_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CodexTotals {
    input: i64,
    output: i64,
    cached: i64,
    reasoning: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexActivityPhase {
    Streaming,
    Settling,
    Preparing,
    Idle,
}

impl CodexActivityPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Settling => "settling",
            Self::Preparing => "preparing",
            Self::Idle => "idle",
        }
    }

    pub fn priority(self) -> u8 {
        match self {
            Self::Streaming => 0,
            Self::Settling => 1,
            Self::Preparing => 2,
            Self::Idle => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexSessionKind {
    Interactive,
    Headless,
}

impl CodexSessionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Headless => "headless",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCurrentSession {
    pub client: String,
    pub session_id: String,
    pub model_id: String,
    pub provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    pub session_kind: CodexSessionKind,
    pub phase: CodexActivityPhase,
    pub last_event_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_token_at: Option<i64>,
    pub total_tokens: i64,
    pub recent_tokens: i64,
}

impl CodexTotals {
    fn from_usage(usage: &CodexTokenUsage) -> Self {
        Self {
            input: usage.input_tokens.unwrap_or(0).max(0),
            output: usage.output_tokens.unwrap_or(0).max(0),
            cached: usage
                .cached_input_tokens
                .unwrap_or(0)
                .max(usage.cache_read_input_tokens.unwrap_or(0))
                .max(0),
            reasoning: usage.reasoning_output_tokens.unwrap_or(0).max(0),
        }
    }

    fn delta_from(self, previous: Self) -> Option<Self> {
        if self.input < previous.input
            || self.output < previous.output
            || self.cached < previous.cached
            || self.reasoning < previous.reasoning
        {
            return None;
        }

        Some(Self {
            input: self.input - previous.input,
            output: self.output - previous.output,
            cached: self.cached - previous.cached,
            reasoning: self.reasoning - previous.reasoning,
        })
    }

    fn saturating_add(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
            cached: self.cached.saturating_add(other.cached),
            reasoning: self.reasoning.saturating_add(other.reasoning),
        }
    }

    fn total(self) -> i64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cached)
            .saturating_add(self.reasoning)
    }

    fn looks_like_stale_regression(self, previous: Self, last: Self) -> bool {
        let previous_total = previous.total();
        let current_total = self.total();
        let last_total = last.total();

        if previous_total <= 0 || current_total <= 0 || last_total <= 0 {
            return false;
        }

        // Some Codex token_count snapshots arrive slightly out of order: the cumulative
        // total regresses by roughly one recent increment, then resumes from the true
        // higher watermark on the next row. Treat those as stale snapshots rather than
        // hard resets so we do not count `last_token_usage` twice.
        current_total.saturating_mul(100) >= previous_total.saturating_mul(98)
            || current_total.saturating_add(last_total.saturating_mul(2)) >= previous_total
    }

    fn into_tokens(self) -> TokenBreakdown {
        // Clamp cached to not exceed input to prevent inflated totals when
        // malformed data reports more cached tokens than input tokens.
        let clamped_cached = self.cached.min(self.input).max(0);
        TokenBreakdown {
            input: (self.input - clamped_cached).max(0),
            output: self.output.max(0),
            cache_read: clamped_cached,
            cache_write: 0,
            reasoning: self.reasoning.max(0),
        }
    }
}

/// Parse a Codex JSONL file with stateful tracking
pub fn parse_codex_file(path: &Path) -> Vec<UnifiedMessage> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let fallback_timestamp = file_modified_timestamp_ms(path);

    let reader = BufReader::new(file);
    let mut messages = Vec::with_capacity(64);
    let mut buffer = Vec::with_capacity(4096);

    let mut current_model: Option<String> = None;
    let mut previous_totals: Option<CodexTotals> = None;
    let mut session_is_headless = false;
    let mut session_provider: Option<String> = None;
    let mut session_agent: Option<String> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut handled = false;
        buffer.clear();
        buffer.extend_from_slice(trimmed.as_bytes());
        if let Ok(entry) = simd_json::from_slice::<CodexEntry>(&mut buffer) {
            if let Some(payload) = entry.payload {
                if entry.entry_type == "session_meta" {
                    if payload.source.as_deref() == Some("exec") {
                        session_is_headless = true;
                    }
                    if let Some(ref provider) = payload.model_provider {
                        session_provider = Some(provider.clone());
                    }
                    if let Some(ref nickname) = payload.agent_nickname {
                        session_agent = Some(nickname.clone());
                    }
                }
                // Extract model from turn_context
                if entry.entry_type == "turn_context" {
                    current_model = extract_model(&payload);
                    handled = true;
                }

                // Process token_count events
                if entry.entry_type == "event_msg"
                    && payload.payload_type.as_deref() == Some("token_count")
                {
                    let Some((model, tokens)) = parse_token_count_payload(
                        &payload,
                        &mut current_model,
                        &mut previous_totals,
                    ) else {
                        continue;
                    };

                    let timestamp = entry
                        .timestamp
                        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(&ts).ok())
                        .map(|dt| dt.timestamp_millis())
                        .unwrap_or(fallback_timestamp);

                    let agent = codex_agent_name(session_is_headless, &session_agent);

                    let provider = session_provider.as_deref().unwrap_or("openai");

                    messages.push(UnifiedMessage::new_with_agent(
                        "codex",
                        model,
                        provider,
                        session_id.clone(),
                        timestamp,
                        tokens,
                        0.0,
                        agent,
                    ));
                    handled = true;
                }
            }

            // Mark session_meta as handled (even if payload was processed above)
            if entry.entry_type == "session_meta" {
                handled = true;
            }
        }

        if handled {
            continue;
        }

        if let Some(msg) = parse_codex_headless_line(
            trimmed,
            &session_id,
            &mut current_model,
            fallback_timestamp,
            session_provider.as_deref(),
            &session_agent,
            session_is_headless,
        ) {
            messages.push(msg);
        }
    }

    messages
}

pub fn parse_codex_current_session(
    path: &Path,
    now_ms: i64,
    path_is_headless: bool,
) -> Option<CodexCurrentSession> {
    let file = std::fs::File::open(path).ok()?;

    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let fallback_timestamp = file_modified_timestamp_ms(path);

    let mut current_model: Option<String> = None;
    let mut previous_totals: Option<CodexTotals> = None;
    let mut session_is_headless = path_is_headless;
    let mut session_provider: Option<String> = None;
    let mut session_agent: Option<String> = None;
    let mut session_cwd: Option<String> = None;
    let mut last_event_at: Option<i64> = None;
    let mut last_token_at: Option<i64> = None;
    let mut total_tokens = 0_i64;
    let mut recent_tokens = 0_i64;

    let reader = BufReader::new(file);
    let mut buffer = Vec::with_capacity(4096);

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut handled = false;
        buffer.clear();
        buffer.extend_from_slice(trimmed.as_bytes());
        if let Ok(entry) = simd_json::from_slice::<CodexEntry>(&mut buffer) {
            let timestamp = entry
                .timestamp
                .as_deref()
                .and_then(super::utils::parse_timestamp_str)
                .unwrap_or(fallback_timestamp);
            last_event_at = Some(last_event_at.map_or(timestamp, |prev| prev.max(timestamp)));

            if let Some(payload) = entry.payload {
                if entry.entry_type == "session_meta" {
                    if payload.source.as_deref() == Some("exec") {
                        session_is_headless = true;
                    }
                    if let Some(provider) =
                        payload.model_provider.as_ref().filter(|s| !s.is_empty())
                    {
                        session_provider = Some(provider.clone());
                    }
                    if let Some(agent) = payload.agent_nickname.as_ref().filter(|s| !s.is_empty()) {
                        session_agent = Some(agent.clone());
                    }
                    if let Some(cwd) = payload.cwd.as_ref().filter(|s| !s.is_empty()) {
                        session_cwd = Some(cwd.clone());
                    }
                    handled = true;
                }

                if entry.entry_type == "turn_context" {
                    if let Some(model) = extract_model(&payload) {
                        current_model = Some(model);
                    }
                    if let Some(cwd) = payload.cwd.as_ref().filter(|s| !s.is_empty()) {
                        session_cwd = Some(cwd.clone());
                    }
                    handled = true;
                }

                if entry.entry_type == "event_msg"
                    && payload.payload_type.as_deref() == Some("token_count")
                {
                    if let Some(cwd) = payload.cwd.as_ref().filter(|s| !s.is_empty()) {
                        session_cwd = Some(cwd.clone());
                    }
                    if let Some((model, tokens)) = parse_token_count_payload(
                        &payload,
                        &mut current_model,
                        &mut previous_totals,
                    ) {
                        current_model = Some(model);
                        let token_total = tokens.total();
                        total_tokens = total_tokens.saturating_add(token_total);
                        if age_ms(now_ms, timestamp) <= RECENT_TOKENS_WINDOW_MS {
                            recent_tokens = recent_tokens.saturating_add(token_total);
                        }
                        last_token_at =
                            Some(last_token_at.map_or(timestamp, |prev| prev.max(timestamp)));
                    }
                    handled = true;
                }
            }

            if handled {
                continue;
            }
        }

        if let Some(event) = parse_codex_headless_event(trimmed, fallback_timestamp) {
            last_event_at =
                Some(last_event_at.map_or(event.timestamp, |prev| prev.max(event.timestamp)));

            if let Some(model) = event.model {
                current_model = Some(model);
            }

            let token_total = event.tokens.total();
            if token_total > 0 {
                total_tokens = total_tokens.saturating_add(token_total);
                if age_ms(now_ms, event.timestamp) <= RECENT_TOKENS_WINDOW_MS {
                    recent_tokens = recent_tokens.saturating_add(token_total);
                }
                last_token_at =
                    Some(last_token_at.map_or(event.timestamp, |prev| prev.max(event.timestamp)));
            }
        }
    }

    let last_event_at = last_event_at?;
    if age_ms(now_ms, last_event_at) > ACTIVE_SESSION_WINDOW_MS {
        return None;
    }

    let session_kind = if session_is_headless {
        CodexSessionKind::Headless
    } else {
        CodexSessionKind::Interactive
    };

    Some(CodexCurrentSession {
        client: "codex".to_string(),
        session_id,
        model_id: current_model.unwrap_or_else(|| "unknown".to_string()),
        provider_id: session_provider.unwrap_or_else(|| "openai".to_string()),
        agent: codex_agent_name(session_is_headless, &session_agent),
        repo_name: repo_name_from_cwd(session_cwd.as_deref()),
        cwd: session_cwd,
        session_kind,
        phase: derive_phase(now_ms, last_event_at, last_token_at),
        last_event_at,
        last_token_at,
        total_tokens,
        recent_tokens,
    })
}

fn parse_token_count_payload(
    payload: &CodexPayload,
    current_model: &mut Option<String>,
    previous_totals: &mut Option<CodexTotals>,
) -> Option<(String, TokenBreakdown)> {
    if let Some(model) = extract_model(payload) {
        *current_model = Some(model);
    }

    let info = payload.info.as_ref()?;

    if let Some(model) = info.model.clone().or(info.model_name.clone()) {
        *current_model = Some(model);
    }

    let model = current_model
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    let total_usage = info.total_token_usage.as_ref().map(CodexTotals::from_usage);
    let last_usage = info.last_token_usage.as_ref().map(CodexTotals::from_usage);

    let (tokens, next_totals) = match (total_usage, last_usage, *previous_totals) {
        (Some(total), Some(last), Some(previous)) => {
            if total == previous {
                return None;
            }
            if total.delta_from(previous).is_none()
                && total.looks_like_stale_regression(previous, last)
            {
                return None;
            }
            (last.into_tokens(), Some(total))
        }
        (Some(total), Some(last), None) => (last.into_tokens(), Some(total)),
        (Some(total), None, Some(previous)) => {
            if total == previous {
                return None;
            }
            if let Some(delta) = total.delta_from(previous) {
                (delta.into_tokens(), Some(total))
            } else {
                *previous_totals = Some(total);
                return None;
            }
        }
        (Some(total), None, None) => (total.into_tokens(), Some(total)),
        (None, Some(last), Some(previous)) => {
            (last.into_tokens(), Some(previous.saturating_add(last)))
        }
        (None, Some(last), None) => (last.into_tokens(), None),
        (None, None, _) => return None,
    };

    if tokens.input == 0 && tokens.output == 0 && tokens.cache_read == 0 && tokens.reasoning == 0 {
        return None;
    }

    *previous_totals = next_totals;

    Some((model, tokens))
}

fn extract_model(payload: &CodexPayload) -> Option<String> {
    payload
        .model_info
        .as_ref()
        .and_then(|mi| mi.slug.clone())
        .filter(|s| !s.is_empty())
        .or(payload.model.clone().filter(|s| !s.is_empty()))
        .or(payload.model_name.clone().filter(|s| !s.is_empty()))
        .or(payload
            .info
            .as_ref()
            .and_then(|i| i.model.clone())
            .filter(|s| !s.is_empty()))
        .or(payload
            .info
            .as_ref()
            .and_then(|i| i.model_name.clone())
            .filter(|s| !s.is_empty()))
}

struct CodexHeadlessUsage {
    input: i64,
    output: i64,
    cached: i64,
    model: Option<String>,
    timestamp_ms: Option<i64>,
}

struct CodexHeadlessEvent {
    model: Option<String>,
    timestamp: i64,
    tokens: TokenBreakdown,
}

fn parse_codex_headless_line(
    line: &str,
    session_id: &str,
    current_model: &mut Option<String>,
    fallback_timestamp: i64,
    session_provider: Option<&str>,
    session_agent: &Option<String>,
    session_is_headless: bool,
) -> Option<UnifiedMessage> {
    let event = parse_codex_headless_event(line, fallback_timestamp)?;
    if let Some(model) = event.model.clone() {
        *current_model = Some(model);
    }

    let model = event
        .model
        .or_else(|| current_model.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let provider = session_provider.unwrap_or("openai");
    let agent = if session_is_headless {
        Some("headless".to_string())
    } else {
        session_agent.clone()
    };

    Some(UnifiedMessage::new_with_agent(
        "codex",
        model,
        provider,
        session_id.to_string(),
        event.timestamp,
        event.tokens,
        0.0,
        agent,
    ))
}

fn parse_codex_headless_event(line: &str, fallback_timestamp: i64) -> Option<CodexHeadlessEvent> {
    let mut bytes = line.as_bytes().to_vec();
    let value: Value = simd_json::from_slice(&mut bytes).ok()?;

    let model = extract_model_from_value(&value)
        .or_else(|| value.get("data").and_then(extract_model_from_value));
    let usage = extract_headless_usage(&value)?;
    let timestamp = usage.timestamp_ms.unwrap_or(fallback_timestamp);

    if usage.input == 0 && usage.output == 0 && usage.cached == 0 {
        return None;
    }

    Some(CodexHeadlessEvent {
        model: usage.model.or(model),
        timestamp,
        tokens: TokenBreakdown {
            input: usage.input.max(0),
            output: usage.output.max(0),
            cache_read: usage.cached.max(0),
            cache_write: 0,
            reasoning: 0,
        },
    })
}

fn codex_agent_name(session_is_headless: bool, session_agent: &Option<String>) -> Option<String> {
    if session_is_headless {
        Some("headless".to_string())
    } else {
        session_agent.clone().or_else(|| Some("Codex".to_string()))
    }
}

fn extract_headless_usage(value: &Value) -> Option<CodexHeadlessUsage> {
    let usage = value
        .get("usage")
        .or_else(|| value.get("data").and_then(|data| data.get("usage")))
        .or_else(|| value.get("result").and_then(|data| data.get("usage")))
        .or_else(|| value.get("response").and_then(|data| data.get("usage")))?;

    let input_tokens = extract_i64(usage.get("input_tokens"))
        .or_else(|| extract_i64(usage.get("prompt_tokens")))
        .or_else(|| extract_i64(usage.get("input")))
        .unwrap_or(0);
    let output_tokens = extract_i64(usage.get("output_tokens"))
        .or_else(|| extract_i64(usage.get("completion_tokens")))
        .or_else(|| extract_i64(usage.get("output")))
        .unwrap_or(0);
    let cached_tokens = extract_i64(usage.get("cached_input_tokens"))
        .or_else(|| extract_i64(usage.get("cache_read_input_tokens")))
        .or_else(|| extract_i64(usage.get("cached_tokens")))
        .unwrap_or(0);

    let model = extract_model_from_value(value)
        .or_else(|| value.get("data").and_then(extract_model_from_value));
    let timestamp_ms = extract_timestamp_from_value(value);

    Some(CodexHeadlessUsage {
        input: input_tokens.saturating_sub(cached_tokens),
        output: output_tokens,
        cached: cached_tokens,
        model,
        timestamp_ms,
    })
}

fn extract_model_from_value(value: &Value) -> Option<String> {
    extract_string(value.get("model"))
        .or_else(|| extract_string(value.get("model_name")))
        .or_else(|| {
            value
                .get("data")
                .and_then(|data| extract_string(data.get("model")))
        })
        .or_else(|| {
            value
                .get("data")
                .and_then(|data| extract_string(data.get("model_name")))
        })
        .or_else(|| {
            value
                .get("response")
                .and_then(|data| extract_string(data.get("model")))
        })
}

fn extract_timestamp_from_value(value: &Value) -> Option<i64> {
    value
        .get("timestamp")
        .or_else(|| value.get("time"))
        .or_else(|| value.get("created_at"))
        .or_else(|| value.get("data").and_then(|data| data.get("timestamp")))
        .and_then(parse_timestamp_value)
}

fn repo_name_from_cwd(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?;
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return None;
    }

    Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn age_ms(now_ms: i64, timestamp_ms: i64) -> i64 {
    if now_ms >= timestamp_ms {
        now_ms - timestamp_ms
    } else {
        0
    }
}

fn derive_phase(now_ms: i64, last_event_at: i64, last_token_at: Option<i64>) -> CodexActivityPhase {
    if let Some(last_token_at) = last_token_at {
        let token_age = age_ms(now_ms, last_token_at);
        if token_age <= STREAMING_WINDOW_MS {
            return CodexActivityPhase::Streaming;
        }
        if token_age <= SETTLING_WINDOW_MS {
            return CodexActivityPhase::Settling;
        }
    }

    if age_ms(now_ms, last_event_at) <= PREPARING_WINDOW_MS {
        CodexActivityPhase::Preparing
    } else {
        CodexActivityPhase::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_test_file(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn test_headless_usage_line() {
        let content = r#"{"type":"turn.completed","model":"gpt-4o-mini","usage":{"input_tokens":120,"cached_input_tokens":20,"output_tokens":30}}"#;
        let file = create_test_file(content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "gpt-4o-mini");
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[0].tokens.output, 30);
        assert_eq!(messages[0].tokens.cache_read, 20);
    }

    #[test]
    fn test_headless_usage_nested_data() {
        let content = r#"{"type":"result","data":{"model_name":"gpt-4o","usage":{"input_tokens":50,"cached_input_tokens":5,"output_tokens":12}}}"#;
        let file = create_test_file(content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "gpt-4o");
        assert_eq!(messages[0].tokens.input, 45);
        assert_eq!(messages[0].tokens.output, 12);
        assert_eq!(messages[0].tokens.cache_read, 5);
    }

    #[test]
    fn test_session_meta_exec_marks_headless() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"originator":"codex_exec","source":"exec"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#;
        let content = format!("{}\n{}", line1, line2);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].agent.as_deref(), Some("headless"));
    }

    #[test]
    fn test_token_count_uses_total_deltas_when_totals_repeat() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5}}}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5}}}}"#;
        let content = format!("{}\n{}\n{}", line1, line2, line3);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 80);
        assert_eq!(messages[0].tokens.output, 30);
        assert_eq!(messages[0].tokens.cache_read, 20);
        assert_eq!(messages[0].tokens.reasoning, 5);
    }

    #[test]
    fn test_token_count_falls_back_to_last_usage_when_totals_reset() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5}}}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#;
        let content = format!("{}\n{}\n{}", line1, line2, line3);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 80);
        assert_eq!(messages[0].tokens.output, 30);
        assert_eq!(messages[0].tokens.cache_read, 20);
        assert_eq!(messages[0].tokens.reasoning, 5);
        assert_eq!(messages[1].tokens.input, 8);
        assert_eq!(messages[1].tokens.output, 3);
        assert_eq!(messages[1].tokens.cache_read, 2);
        assert_eq!(messages[1].tokens.reasoning, 1);
    }

    #[test]
    fn test_token_count_advances_baseline_after_missing_total_fallback() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5}}}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#;
        let line4 = r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":110,"cached_input_tokens":22,"output_tokens":33,"reasoning_output_tokens":6},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#;
        let content = format!("{}\n{}\n{}\n{}", line1, line2, line3, line4);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 80);
        assert_eq!(messages[0].tokens.output, 30);
        assert_eq!(messages[0].tokens.cache_read, 20);
        assert_eq!(messages[0].tokens.reasoning, 5);
        assert_eq!(messages[1].tokens.input, 8);
        assert_eq!(messages[1].tokens.output, 3);
        assert_eq!(messages[1].tokens.cache_read, 2);
        assert_eq!(messages[1].tokens.reasoning, 1);
    }

    #[test]
    fn test_token_count_skips_regressed_totals_without_last_usage() {
        // When totals regress and last_usage is absent, the row should be
        // skipped entirely to avoid double-counting the full cumulative total.
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5}}}}"#;
        // Totals regress (lower values) and no last_token_usage — should skip
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":50,"cached_input_tokens":10,"output_tokens":15,"reasoning_output_tokens":2}}}}"#;
        // Normal continuation after reset
        let line4 = r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":80,"cached_input_tokens":15,"output_tokens":25,"reasoning_output_tokens":4}}}}"#;
        let content = format!("{}\n{}\n{}\n{}", line1, line2, line3, line4);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        // Should produce 2 messages: first from line2 (full total),
        // then delta from line4 relative to line3 (baseline reset).
        assert_eq!(messages.len(), 2);
        // First message: full total
        assert_eq!(messages[0].tokens.input, 80);
        assert_eq!(messages[0].tokens.output, 30);
        assert_eq!(messages[0].tokens.cache_read, 20);
        assert_eq!(messages[0].tokens.reasoning, 5);
        // Second message: delta from 50→80
        assert_eq!(messages[1].tokens.input, 25);
        assert_eq!(messages[1].tokens.output, 10);
        assert_eq!(messages[1].tokens.cache_read, 5);
        assert_eq!(messages[1].tokens.reasoning, 2);
    }

    #[test]
    fn test_into_tokens_clamps_cached_to_input() {
        // When cached > input (malformed data), cached should be clamped to input
        // so that input + cache_read never exceeds the raw input value.
        let totals = CodexTotals {
            input: 50,
            output: 30,
            cached: 100, // More than input — malformed
            reasoning: 5,
        };
        let tokens = totals.into_tokens();
        assert_eq!(tokens.cache_read, 50); // Clamped to input
        assert_eq!(tokens.input, 0); // input - clamped_cached = 0
        assert_eq!(tokens.output, 30);
        assert_eq!(tokens.reasoning, 5);
    }

    #[test]
    fn test_token_count_ignores_negative_fallback_usage_in_baseline() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5}}}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":-10,"cached_input_tokens":-2,"output_tokens":-3,"reasoning_output_tokens":-1}}}}"#;
        let line4 = r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":110,"cached_input_tokens":22,"output_tokens":33,"reasoning_output_tokens":6},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#;
        let content = format!("{}\n{}\n{}\n{}", line1, line2, line3, line4);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 80);
        assert_eq!(messages[0].tokens.output, 30);
        assert_eq!(messages[0].tokens.cache_read, 20);
        assert_eq!(messages[0].tokens.reasoning, 5);
        assert_eq!(messages[1].tokens.input, 8);
        assert_eq!(messages[1].tokens.output, 3);
        assert_eq!(messages[1].tokens.cache_read, 2);
        assert_eq!(messages[1].tokens.reasoning, 1);
    }

    #[test]
    fn test_token_count_avoids_double_counting_stale_cumulative_regressions() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5}}}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":110,"cached_input_tokens":22,"output_tokens":33,"reasoning_output_tokens":6},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#;
        let line4 = r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":109,"cached_input_tokens":21,"output_tokens":32,"reasoning_output_tokens":6},"last_token_usage":{"input_tokens":9,"cached_input_tokens":1,"output_tokens":2,"reasoning_output_tokens":0}}}}"#;
        let line5 = r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":119,"cached_input_tokens":23,"output_tokens":35,"reasoning_output_tokens":6},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":0}}}}"#;
        let content = format!("{}\n{}\n{}\n{}\n{}", line1, line2, line3, line4, line5);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].tokens.input, 80);
        assert_eq!(messages[0].tokens.output, 30);
        assert_eq!(messages[0].tokens.cache_read, 20);
        assert_eq!(messages[0].tokens.reasoning, 5);

        assert_eq!(messages[1].tokens.input, 8);
        assert_eq!(messages[1].tokens.output, 3);
        assert_eq!(messages[1].tokens.cache_read, 2);
        assert_eq!(messages[1].tokens.reasoning, 1);

        // Stale snapshot (line4) is now skipped entirely; messages[2]
        // comes from line5's last_token_usage instead.
        assert_eq!(messages[2].tokens.input, 8);
        assert_eq!(messages[2].tokens.output, 3);
        assert_eq!(messages[2].tokens.cache_read, 2);
        assert_eq!(messages[2].tokens.reasoning, 0);
    }

    #[test]
    fn test_token_count_handles_multiple_stale_regressions_before_recovery() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5},"last_token_usage":{"input_tokens":100,"cached_input_tokens":20,"output_tokens":30,"reasoning_output_tokens":5}}}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":110,"cached_input_tokens":22,"output_tokens":33,"reasoning_output_tokens":6},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#;
        let line4 = r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":109,"cached_input_tokens":21,"output_tokens":32,"reasoning_output_tokens":6},"last_token_usage":{"input_tokens":9,"cached_input_tokens":1,"output_tokens":2,"reasoning_output_tokens":0}}}}"#;
        let line5 = r#"{"timestamp":"2026-01-01T00:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":118,"cached_input_tokens":22,"output_tokens":34,"reasoning_output_tokens":6},"last_token_usage":{"input_tokens":9,"cached_input_tokens":1,"output_tokens":2,"reasoning_output_tokens":0}}}}"#;
        let line6 = r#"{"timestamp":"2026-01-01T00:00:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":128,"cached_input_tokens":24,"output_tokens":37,"reasoning_output_tokens":6},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":0}}}}"#;
        let content = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            line1, line2, line3, line4, line5, line6
        );
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        // Stale line4 is skipped; messages come from lines 2, 3, 5, 6.
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].tokens.input, 80);
        assert_eq!(messages[1].tokens.input, 8);
        assert_eq!(messages[2].tokens.input, 8);
        assert_eq!(messages[2].tokens.output, 2);
        assert_eq!(messages[2].tokens.cache_read, 1);
        assert_eq!(messages[2].tokens.reasoning, 0);
        assert_eq!(messages[3].tokens.input, 8);
        assert_eq!(messages[3].tokens.output, 3);
        assert_eq!(messages[3].tokens.cache_read, 2);
        assert_eq!(messages[3].tokens.reasoning, 0);
    }

    #[test]
    fn test_token_count_treats_large_regressions_as_real_resets() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10000,"cached_input_tokens":1000,"output_tokens":400,"reasoning_output_tokens":50},"last_token_usage":{"input_tokens":10000,"cached_input_tokens":1000,"output_tokens":400,"reasoning_output_tokens":50}}}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":7600,"cached_input_tokens":800,"output_tokens":280,"reasoning_output_tokens":35},"last_token_usage":{"input_tokens":25,"cached_input_tokens":5,"output_tokens":4,"reasoning_output_tokens":1}}}}"#;
        let line4 = r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":7625,"cached_input_tokens":805,"output_tokens":284,"reasoning_output_tokens":36},"last_token_usage":{"input_tokens":25,"cached_input_tokens":5,"output_tokens":4,"reasoning_output_tokens":1}}}}"#;
        let content = format!("{}\n{}\n{}\n{}", line1, line2, line3, line4);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].tokens.input, 9000);
        assert_eq!(messages[0].tokens.output, 400);
        assert_eq!(messages[0].tokens.cache_read, 1000);
        assert_eq!(messages[0].tokens.reasoning, 50);

        assert_eq!(messages[1].tokens.input, 20);
        assert_eq!(messages[1].tokens.output, 4);
        assert_eq!(messages[1].tokens.cache_read, 5);
        assert_eq!(messages[1].tokens.reasoning, 1);

        assert_eq!(messages[2].tokens.input, 20);
        assert_eq!(messages[2].tokens.output, 4);
        assert_eq!(messages[2].tokens.cache_read, 5);
        assert_eq!(messages[2].tokens.reasoning, 1);
    }

    #[test]
    fn test_first_event_uses_last_not_total_for_resumed_sessions() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":5000,"cached_input_tokens":500,"output_tokens":800,"reasoning_output_tokens":100},"last_token_usage":{"input_tokens":12,"cached_input_tokens":2,"output_tokens":5,"reasoning_output_tokens":1}}}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":5012,"cached_input_tokens":502,"output_tokens":805,"reasoning_output_tokens":101},"last_token_usage":{"input_tokens":12,"cached_input_tokens":2,"output_tokens":5,"reasoning_output_tokens":1}}}}"#;
        let content = format!("{}\n{}\n{}", line1, line2, line3);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 10);
        assert_eq!(messages[0].tokens.output, 5);
        assert_eq!(messages[0].tokens.cache_read, 2);
        assert_eq!(messages[0].tokens.reasoning, 1);
        assert_eq!(messages[1].tokens.input, 10);
        assert_eq!(messages[1].tokens.output, 5);
        assert_eq!(messages[1].tokens.cache_read, 2);
        assert_eq!(messages[1].tokens.reasoning, 1);
    }

    #[test]
    fn test_zero_token_snapshot_does_not_inflate_later_deltas() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":50,"output_tokens":80,"reasoning_output_tokens":10},"last_token_usage":{"input_tokens":500,"cached_input_tokens":50,"output_tokens":80,"reasoning_output_tokens":10}}}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0},"last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0}}}}"#;
        let line4 = r#"{"timestamp":"2026-01-01T00:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":510,"cached_input_tokens":52,"output_tokens":83,"reasoning_output_tokens":11},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#;
        let content = format!("{}\n{}\n{}\n{}", line1, line2, line3, line4);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 450);
        assert_eq!(messages[0].tokens.output, 80);
        assert_eq!(messages[0].tokens.cache_read, 50);
        assert_eq!(messages[0].tokens.reasoning, 10);
        assert_eq!(messages[1].tokens.input, 8);
        assert_eq!(messages[1].tokens.output, 3);
        assert_eq!(messages[1].tokens.cache_read, 2);
        assert_eq!(messages[1].tokens.reasoning, 1);
    }

    #[test]
    fn test_model_info_slug_from_turn_context() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model_info":{"slug":"o3-pro"}}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#;
        let content = format!("{}\n{}", line1, line2);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "o3-pro");
    }

    #[test]
    fn test_session_meta_provider_and_agent() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"source":"interactive","model_provider":"azure","agent_nickname":"my-agent"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#;
        let content = format!("{}\n{}\n{}", line1, line2, line3);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].provider_id, "azure");
        assert_eq!(messages[0].agent.as_deref(), Some("my-agent"));
    }

    #[test]
    fn test_interactive_sessions_without_agent_nickname_fall_back_to_codex() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"source":"interactive","model_provider":"openai"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#;
        let content = format!("{}\n{}\n{}", line1, line2, line3);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].agent.as_deref(), Some("Codex"));
    }

    #[test]
    fn test_cached_tokens_takes_max_of_both_fields() {
        let usage = CodexTokenUsage {
            input_tokens: Some(100),
            output_tokens: Some(30),
            cached_input_tokens: Some(10),
            cache_read_input_tokens: Some(20),
            reasoning_output_tokens: Some(5),
        };
        let totals = CodexTotals::from_usage(&usage);
        assert_eq!(totals.cached, 20);
    }

    #[test]
    fn test_compaction_total_drop_uses_last_as_increment() {
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":150000,"cached_input_tokens":10000,"output_tokens":20000,"reasoning_output_tokens":5000},"last_token_usage":{"input_tokens":150000,"cached_input_tokens":10000,"output_tokens":20000,"reasoning_output_tokens":5000}}}}"#;
        let line3 = r#"{"timestamp":"2026-01-01T00:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200000,"cached_input_tokens":15000,"output_tokens":25000,"reasoning_output_tokens":6000},"last_token_usage":{"input_tokens":50,"cached_input_tokens":5,"output_tokens":10,"reasoning_output_tokens":2}}}}"#;
        let content = format!("{}\n{}\n{}", line1, line2, line3);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].tokens.input, 45);
        assert_eq!(messages[1].tokens.output, 10);
        assert_eq!(messages[1].tokens.cache_read, 5);
        assert_eq!(messages[1].tokens.reasoning, 2);
    }

    #[test]
    fn test_headless_fallback_uses_session_provider_and_agent() {
        // session_meta sets provider to "azure" and agent to "my-bot",
        // then a line falls through to headless parsing (no structured entry_type)
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"model_provider":"azure","agent_nickname":"my-bot"}}"#;
        let line2 = r#"{"type":"turn.completed","model":"gpt-4o","usage":{"input_tokens":100,"output_tokens":50}}"#;
        let content = format!("{}\n{}", line1, line2);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].provider_id, "azure");
        assert_eq!(messages[0].agent.as_deref(), Some("my-bot"));
    }

    #[test]
    fn test_headless_fallback_defaults_to_openai_without_session_meta() {
        // No session_meta — headless fallback should default to "openai"
        let content = r#"{"type":"turn.completed","model":"gpt-4o-mini","usage":{"input_tokens":120,"cached_input_tokens":20,"output_tokens":30}}"#;
        let file = create_test_file(content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].provider_id, "openai");
        assert!(messages[0].agent.is_none());
    }

    #[test]
    fn test_extract_model_skips_empty_slug_falls_through_to_model() {
        // model_info.slug is empty string, but payload.model has a valid value.
        // extract_model should skip the empty slug and return payload.model.
        let line1 = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model_info":{"slug":""},"model":"gpt-4o"}}"#;
        let line2 = r#"{"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#;
        let content = format!("{}\n{}", line1, line2);
        let file = create_test_file(&content);

        let messages = parse_codex_file(file.path());

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "gpt-4o");
    }

    #[test]
    fn test_parse_codex_current_session_streaming_with_repo_and_recent_tokens() {
        let content = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"source":"interactive","model_provider":"openai","cwd":"/tmp/tokscale","agent_nickname":"Codex"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:01Z","type":"turn_context","payload":{"model":"gpt-5.3-codex"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:58Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3}}}}"#
        );
        let file = create_test_file(content);

        let session = parse_codex_current_session(file.path(), 1_767_225_660_000, false).unwrap();

        assert_eq!(session.model_id, "gpt-5.3-codex");
        assert_eq!(session.provider_id, "openai");
        assert_eq!(session.cwd.as_deref(), Some("/tmp/tokscale"));
        assert_eq!(session.repo_name.as_deref(), Some("tokscale"));
        assert_eq!(session.session_kind, CodexSessionKind::Interactive);
        assert_eq!(session.phase, CodexActivityPhase::Streaming);
        assert_eq!(session.total_tokens, 13);
        assert_eq!(session.recent_tokens, 13);
    }

    #[test]
    fn test_parse_codex_current_session_preparing_without_tokens() {
        let content = concat!(
            r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"source":"interactive","cwd":"/tmp/project"}}"#,
            "\n",
            r#"{"timestamp":"2026-01-01T00:00:04Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#
        );
        let file = create_test_file(content);

        let session = parse_codex_current_session(file.path(), 1_767_225_608_000, false).unwrap();

        assert_eq!(session.phase, CodexActivityPhase::Preparing);
        assert_eq!(session.total_tokens, 0);
        assert_eq!(session.last_token_at, None);
    }

    #[test]
    fn test_parse_codex_current_session_ignores_old_sessions() {
        let content = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"turn_context","payload":{"model":"gpt-5.2"}}"#;
        let file = create_test_file(content);

        let session = parse_codex_current_session(file.path(), 1_767_226_201_000, false);

        assert!(session.is_none());
    }

    #[test]
    fn test_parse_codex_current_session_marks_headless_from_path() {
        let content = r#"{"type":"turn.completed","model":"gpt-4o-mini","usage":{"input_tokens":120,"cached_input_tokens":20,"output_tokens":30}}"#;
        let file = create_test_file(content);

        let session =
            parse_codex_current_session(file.path(), chrono::Utc::now().timestamp_millis(), true)
                .unwrap();

        assert_eq!(session.session_kind, CodexSessionKind::Headless);
        assert_eq!(session.agent.as_deref(), Some("headless"));
        assert_eq!(session.total_tokens, 150);
    }
}
