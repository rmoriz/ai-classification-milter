//! ai-classification-milter: header (+ body excerpt) -> N decision LLMs parallel -> tag/reject + eval log.
//!
//! Flow: MAIL (reset) -> RCPT* -> HEADER* -> BODY* -> EOM (one fan-out to all models).
//! Fail-open: any LLM error, all-LLM error, or a milter crash -> Postfix accepts
//! (requires milter_default_action=accept / per-milter default_action=accept).

use bytes::Bytes;
use chrono::Local;
use futures::future::join_all;
use indymilter::{
    Actions, Callbacks, Config, Context, ContextActions, EomContext, SetErrorReply, Status,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString},
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::net::TcpListener;
use tracing::{info, warn};

mod dashboard;

// ---------------------------------------------------------------- config

#[derive(Debug, Clone)]
struct ModelEndpoint {
    name: String,
    url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Any,      // reject if >=1 model says spam
    Majority, // reject if a majority of successful answers says spam
    All,      // reject only if all successful answers say spam
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub listen: String,
    pub models: Vec<ModelEndpoint>,
    pub openrouter_url: String,
    pub openrouter_mode: bool,
    pub openrouter_referer: Option<String>,
    pub openrouter_title: Option<String>,
    pub question_instructions: String,
    pub question_true: String,
    pub question_false: String,
    pub llm_api_key: Option<String>,
    pub llm_timeout: Duration,
    pub score_threshold: f32,
    pub decision: Decision,
    pub header_max_count: usize,
    pub header_max_value_chars: usize,
    pub body_max_bytes: usize,
    pub llm_body_chars: usize,
    pub log_spam_dir: PathBuf,
    pub eval_log: PathBuf,
    pub log_all_mails: bool, // true = log every mail as .eml (eval), otherwise only spam
    pub learn_mode: bool,    // true = never reject, only tag + log
    pub whitelist_sender_domains: Vec<String>, // lowercase, no leading dot; subdomains match too
    pub dashboard_enabled: bool,
    pub dashboard_listen: String, // own socket, e.g. 127.0.0.1:8892
    pub dashboard_token: Option<String>, // if set, required as ?token= or Bearer
    pub dashboard_scramble_senders: bool, // blur sender addresses in HTML views
}

const DEFAULT_OPENROUTER_MODELS: &str = "microsoft/microsoft-decision-1,cloudflare/clef-omni,upstage/solar-decide-flash,nace-ai/drex-v1.5,typesafe/jev-1.13";
const DEFAULT_OPENROUTER_URL: &str = "https://openrouter.ai/api/alpha/decisions";
const DEFAULT_QUESTION: &str = "Is this mail spam, phishing or malicious?";
const DEFAULT_CRITERIA_TRUE: &str = "The mail is unsolicited bulk mail, phishing, scam, fraud, or contains malicious content, links, or attachments.";
const DEFAULT_CRITERIA_FALSE: &str = "The mail is legitimate correspondence, an expected newsletter with List-Unsubscribe and aligned authentication, or an expected transactional notification.";
const BUILTIN_SYSTEM_PROMPT: &str = "You are a spam/phishing/malware classifier for incoming email at an MTA. \
Input: envelope metadata, STATE = all message headers, plus a truncated body excerpt. No attachments, no fetched URLs. \
Weight Authentication-Results / Received chain / envelope-vs-header mismatch over Subject keywords. \
Newsletters with List-Unsubscribe and aligned authentication are ham. Never invent truncated content. \
QUESTION: Is this mail spam, phishing or malicious? \
Answer STRICT JSON only, no prose: {\"verdict\": \"yes\" or \"no\", \"percentage\": 0-100 spam probability, \"reason\": \"max 20 words\"}.";

#[allow(dead_code)]
fn parse_models(env: &dyn Fn(&str) -> Option<String>) -> (Vec<ModelEndpoint>, bool) {
    // OpenRouter mode: OPENROUTER_MODELS="id1,id2,..." (default: 5 eval models).
    // All share OPENROUTER_URL + one API key.
    if let Some(raw) = env("OPENROUTER_MODELS") {
        if !raw.trim().is_empty() {
            let url = env("OPENROUTER_URL").unwrap_or_else(|| DEFAULT_OPENROUTER_URL.into());
            let v: Vec<ModelEndpoint> = raw
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|name| ModelEndpoint { name: name.to_string(), url: url.clone() })
                .collect();
            if !v.is_empty() {
                return (v, true);
            }
        }
    } else {
        // Default: OpenRouter eval with the 5 standard models.
        let url = env("OPENROUTER_URL").unwrap_or_else(|| DEFAULT_OPENROUTER_URL.into());
        let v: Vec<ModelEndpoint> = DEFAULT_OPENROUTER_MODELS
            .split(',')
            .map(|name| ModelEndpoint { name: name.to_string(), url: url.clone() })
            .collect();
        return (v, true);
    }
    // Legacy: MODELS="name=url,name2=url2" with a custom {spam,score,reason} endpoint.
    if let Some(raw) = env("MODELS") {
        let v: Vec<ModelEndpoint> = raw
            .split(',')
            .filter_map(|e| {
                let e = e.trim();
                if e.is_empty() {
                    return None;
                }
                let (name, url) = e.split_once('=')?;
                Some(ModelEndpoint { name: name.trim().to_string(), url: url.trim().to_string() })
            })
            .collect();
        if !v.is_empty() {
            return (v, false);
        }
    }
    // Fallback: single legacy model.
    (vec![ModelEndpoint {
        name: env("LLM_MODEL").unwrap_or_else(|| "spam-decision".into()),
        url: env("LLM_URL").unwrap_or_else(|| "http://127.0.0.1:11434/v1/chat/completions".into()),
    }], false)
}

#[allow(dead_code)]
fn load_system_prompt(env: &dyn Fn(&str) -> Option<String>) -> String {
    if let Some(path) = env("SYSTEM_PROMPT_FILE").or_else(|| Some("prompt.md".into())) {
        if let Ok(s) = std::fs::read_to_string(&path) {
            if !s.trim().is_empty() {
                return s;
            }
        }
    }
    BUILTIN_SYSTEM_PROMPT.to_string()
}

// ------------------------------------------------------- config file (TOML)
//
// Precedence: env var > ai-classification-milter.toml > defaults. Secrets (api_key) preferably
// via env (OPENROUTER_API_KEY), path via MILTER_CONFIG (default ./ai-classification-milter.toml,
// fallback ./llm-milter.toml for existing deployments).

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    listen: Option<String>,
    openrouter: Option<OpenRouterSection>,
    limits: Option<LimitsSection>,
    logging: Option<LoggingSection>,
    whitelist: Option<WhitelistSection>,
    dashboard: Option<DashboardSection>,
}

#[derive(Debug, Deserialize, Default)]
struct OpenRouterSection {
    url: Option<String>,
    api_key: Option<String>,
    models: Option<Vec<String>>,
    referer: Option<String>,
    title: Option<String>,
    question: Option<QuestionSection>,
}

#[derive(Debug, Deserialize, Default)]
struct QuestionSection {
    instructions: Option<String>,
    criteria_true: Option<String>,
    criteria_false: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct LimitsSection {
    llm_timeout_secs: Option<u64>,
    score_threshold: Option<f32>,
    decision: Option<String>,
    header_max_count: Option<usize>,
    header_max_value_chars: Option<usize>,
    body_max_bytes: Option<usize>,
    llm_body_chars: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
struct LoggingSection {
    spam_dir: Option<String>,
    eval_log: Option<String>,
    log_all_mails: Option<bool>,
    learn_mode: Option<bool>,
}

#[derive(Debug, Deserialize, Default)]
struct WhitelistSection {
    sender_domains: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
struct DashboardSection {
    enabled: Option<bool>,
    listen: Option<String>,
    token: Option<String>,
    scramble_senders: Option<bool>,
}

fn config_path() -> String {
    if let Ok(p) = std::env::var("MILTER_CONFIG") {
        return p;
    }
    let new = "ai-classification-milter.toml";
    if std::path::Path::new(new).exists() {
        return new.into();
    }
    // Backwards compatibility: old deployments with llm-milter.toml keep working.
    let legacy = "llm-milter.toml";
    if std::path::Path::new(legacy).exists() {
        return legacy.into();
    }
    new.into()
}

fn load_file_config() -> FileConfig {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => match toml::from_str::<FileConfig>(&s) {
            Ok(c) => {
                info!(path = %path, "config file loaded");
                c
            }
            Err(e) => {
                warn!(path = %path, error = %e, "config file parse error, using defaults");
                FileConfig::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
        Err(e) => {
            warn!(path = %path, error = %e, "config file unreadable, using defaults");
            FileConfig::default()
        }
    }
}

impl AppConfig {
    fn load() -> Self {
        let env = |k: &str| std::env::var(k).ok();
        let fc = load_file_config();
        let or = fc.openrouter.unwrap_or_default();
        let lim = fc.limits.unwrap_or_default();
        let log = fc.logging.unwrap_or_default();

        let bool_val = |e: Option<String>, f: Option<bool>, dflt: bool| {
            e.map(|v| v == "1" || v.eq_ignore_ascii_case("true")).or(f).unwrap_or(dflt)
        };
        let decision_src = env("DECISION").or(lim.decision).unwrap_or_else(|| "any".into());
        let decision = match decision_src.as_str() {
            "majority" => Decision::Majority,
            "all" => Decision::All,
            _ => Decision::Any,
        };

        // Legacy mode only when MODELS=name=url,... is set explicitly.
        let models: Vec<ModelEndpoint>;
        let openrouter_mode: bool;
        if let Some(raw) = env("MODELS") {
            let v: Vec<ModelEndpoint> = raw
                .split(',')
                .filter_map(|e| {
                    let e = e.trim();
                    if e.is_empty() {
                        return None;
                    }
                    let (name, url) = e.split_once('=')?;
                    Some(ModelEndpoint { name: name.trim().to_string(), url: url.trim().to_string() })
                })
                .collect();
            if v.is_empty() {
                warn!("MODELS set but unparsable, falling back to OpenRouter");
                (models, openrouter_mode) = openrouter_endpoints(&env, &or);
            } else {
                models = v;
                openrouter_mode = false;
            }
        } else {
            (models, openrouter_mode) = openrouter_endpoints(&env, &or);
        }
        let openrouter_url = env("OPENROUTER_URL")
            .or(or.url)
            .unwrap_or_else(|| DEFAULT_OPENROUTER_URL.into());

        let q = or.question.unwrap_or_default();
        let question_instructions = env("QUESTION_INSTRUCTIONS").or(q.instructions).unwrap_or_else(|| DEFAULT_QUESTION.into());
        let question_true = env("QUESTION_TRUE").or(q.criteria_true).unwrap_or_else(|| DEFAULT_CRITERIA_TRUE.into());
        let question_false = env("QUESTION_FALSE").or(q.criteria_false).unwrap_or_else(|| DEFAULT_CRITERIA_FALSE.into());

        // Whitelist sender domains: env WHITELIST_SENDER_DOMAINS (comma-separated) > TOML [whitelist] > empty.
        let whitelist_sender_domains: Vec<String> = env("WHITELIST_SENDER_DOMAINS")
            .map(|raw| raw.split(',').map(|s| normalize_domain(s)).filter(|s| !s.is_empty()).collect())
            .filter(|v: &Vec<String>| !v.is_empty())
            .or_else(|| {
                fc.whitelist.and_then(|w| w.sender_domains).map(|v| {
                    v.into_iter().map(|s| normalize_domain(&s)).filter(|s| !s.is_empty()).collect()
                })
            })
            .filter(|v: &Vec<String>| !v.is_empty())
            .unwrap_or_default();

        // Dashboard: env > TOML > defaults (default: aus).
        let dash = fc.dashboard.unwrap_or_default();
        let dashboard_enabled = bool_val(env("DASHBOARD_ENABLED"), dash.enabled, false);
        let dashboard_listen = env("DASHBOARD_LISTEN")
            .or(dash.listen)
            .unwrap_or_else(|| "127.0.0.1:8892".into());
        let dashboard_token = env("DASHBOARD_TOKEN").or(dash.token).filter(|t| !t.is_empty());
        let dashboard_scramble_senders = bool_val(env("DASHBOARD_SCRAMBLE_SENDERS"), dash.scramble_senders, false);

        Self {
            listen: env("MILTER_LISTEN").or(fc.listen).unwrap_or_else(|| "127.0.0.1:8891".into()),
            models,
            openrouter_url,
            openrouter_mode,
            openrouter_referer: env("OPENROUTER_REFERER").or(or.referer),
            openrouter_title: env("OPENROUTER_TITLE").or(or.title),
            question_instructions,
            question_true,
            question_false,
            llm_api_key: env("OPENROUTER_API_KEY").or_else(|| env("LLM_API_KEY")).or(or.api_key),
            llm_timeout: Duration::from_secs(
                env("LLM_TIMEOUT_SECS").and_then(|v| v.parse().ok()).or(lim.llm_timeout_secs).unwrap_or(5),
            ),
            score_threshold: env("SCORE_THRESHOLD").and_then(|v| v.parse().ok()).or(lim.score_threshold).unwrap_or(0.85),
            decision,
            header_max_count: env("HEADER_MAX_COUNT").and_then(|v| v.parse().ok()).or(lim.header_max_count).unwrap_or(200),
            header_max_value_chars: env("HEADER_MAX_VALUE_CHARS").and_then(|v| v.parse().ok()).or(lim.header_max_value_chars).unwrap_or(2048),
            body_max_bytes: env("BODY_MAX_BYTES").and_then(|v| v.parse().ok()).or(lim.body_max_bytes).unwrap_or(100_000),
            llm_body_chars: env("LLM_BODY_CHARS").and_then(|v| v.parse().ok()).or(lim.llm_body_chars).unwrap_or(4000),
            log_spam_dir: PathBuf::from(env("LOG_SPAM_DIR").or(log.spam_dir).unwrap_or_else(|| "./spam-quarantine".into())),
            eval_log: PathBuf::from(env("EVAL_LOG").or(log.eval_log).unwrap_or_else(|| "./eval.jsonl".into())),
            log_all_mails: bool_val(env("LOG_ALL_MAILS"), log.log_all_mails, false),
            learn_mode: bool_val(env("LEARN_MODE"), log.learn_mode, true),
            whitelist_sender_domains,
            dashboard_enabled,
            dashboard_listen,
            dashboard_token,
            dashboard_scramble_senders,
        }
    }
}

/// OpenRouter endpoints: env OPENROUTER_MODELS (comma-separated) > TOML models > default 5.
fn openrouter_endpoints(
    env: &dyn Fn(&str) -> Option<String>,
    or: &OpenRouterSection,
) -> (Vec<ModelEndpoint>, bool) {
    let url = env("OPENROUTER_URL")
        .or_else(|| or.url.clone())
        .unwrap_or_else(|| DEFAULT_OPENROUTER_URL.into());
    let names: Vec<String> = env("OPENROUTER_MODELS")
        .map(|raw| raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .filter(|v: &Vec<String>| !v.is_empty())
        .or_else(|| or.models.clone())
        .unwrap_or_else(|| DEFAULT_OPENROUTER_MODELS.split(',').map(str::to_string).collect());
    let v = names.into_iter().map(|name| ModelEndpoint { name, url: url.clone() }).collect();
    (v, true)
}

/// Normalize a domain: trim, strip leading dot and trailing FQDN dot, lowercase.
fn normalize_domain(s: &str) -> String {
    s.trim().trim_start_matches('.').trim_end_matches('.').trim().to_lowercase()
}

/// Reduce an envelope sender ("<user@domain>", "user@domain", "\"Name\" <user@domain>", "<>")
/// to its domain. None for empty/bounce/empty local part.
fn sender_domain(sender: &str) -> Option<String> {
    let s = sender.trim();
    if s.is_empty() || s == "<>" {
        return None;
    }
    // For "Name <addr>" take the last angle-bracket pair.
    let addr = if let (Some(a), Some(b)) = (s.rfind('<'), s.rfind('>')) {
        if a < b { &s[a + 1..b] } else { s }
    } else {
        s.trim_matches(|c| c == '<' || c == '>' || c == '"' || c == '\'' || c == ' ').trim()
    };
    let addr = addr.trim();
    if addr.is_empty() || addr == "<>" {
        return None;
    }
    let (_, domain) = addr.rsplit_once('@')?;
    let d = normalize_domain(domain);
    if d.is_empty() || !d.contains('.') {
        return None;
    }
    Some(d)
}

/// true if the sender domain (exact or subdomain) is whitelisted.
/// Falls back to the From header when the envelope is empty (CLI mode without envelope).
fn is_sender_whitelisted(cfg: &AppConfig, envelope_from: &str, headers: &[(String, String)]) -> bool {
    if cfg.whitelist_sender_domains.is_empty() {
        return false;
    }
    let mut candidates: Vec<String> = Vec::new();
    if let Some(d) = sender_domain(envelope_from) {
        candidates.push(d);
    }
    // Only as a fallback (CLI): evaluate the From header when the envelope is empty/unknown.
    if candidates.is_empty() {
        for (n, v) in headers {
            if n.eq_ignore_ascii_case("from") {
                if let Some(d) = sender_domain(v) {
                    candidates.push(d);
                    break;
                }
            }
        }
    }
    candidates.into_iter().any(|d| {
        cfg.whitelist_sender_domains.iter().any(|w| d == *w || d.ends_with(&format!(".{w}")))
    })
}

// ------------------------------------------------------------- per-mail state

#[derive(Debug, Default)]
struct MailState {
    sender: String,
    recipients: Vec<String>,
    headers: Vec<(String, String)>,
    header_bytes: usize,
    body: Vec<u8>,
    body_truncated: bool,
}

impl MailState {
    fn push_header(&mut self, cfg: &AppConfig, name: &str, value: &str) {
        if self.headers.len() >= cfg.header_max_count {
            return;
        }
        let v: String = value.chars().take(cfg.header_max_value_chars).collect();
        self.header_bytes += name.len() + v.len();
        self.headers.push((name.to_string(), v));
    }

    fn push_body(&mut self, cfg: &AppConfig, chunk: &[u8]) {
        if self.body.len() >= cfg.body_max_bytes {
            self.body_truncated = true;
            return;
        }
        let room = cfg.body_max_bytes - self.body.len();
        if chunk.len() > room {
            self.body.extend_from_slice(&chunk[..room]);
            self.body_truncated = true;
        } else {
            self.body.extend_from_slice(chunk);
        }
    }
}

// ---------------------------------------------------------------- LLM types

#[derive(Debug, Serialize, Clone)]
struct LlmRequest {
    model: String,
    queue_id: String,
    client_addr: String,
    helo: String,
    envelope_from: String,
    envelope_rcpts: Vec<String>,
    headers: Vec<(String, String)>,
    body_excerpt: String,
    body_truncated: bool,
}

#[derive(Debug, Deserialize, Clone, Default, Serialize)]
struct LlmVerdict {
    #[serde(default)]
    spam: bool,
    #[serde(default)]
    score: f32,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone, Serialize)]
struct ModelOutcome {
    model: String,
    #[serde(flatten)]
    verdict: LlmVerdict,
    error: Option<String>,
    latency_ms: u64,
    cost_usd: Option<f64>,
    is_spam: bool, // spam || score >= threshold (only on success)
}

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client")
    })
}

fn get_macro(ctx_macros: &indymilter::Macros, name: &[u8]) -> String {
    let cstr = match CStr::from_bytes_with_nul(name) {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    ctx_macros.get(cstr).map(|v| v.to_string_lossy().into_owned()).unwrap_or_default()
}

// ------------------------------------------------------- Decisions API client
//
// POST {openrouter_url} {model, state, questions:{is_spam:{type:noul,...}}, session_id}
// -> {answers:{is_spam:{type:noul, noul:0..1}}, usage:{cost,...}}
// No free-text parsing: Noul directly returns the yes probability.
// Docs: https://openrouter.ai/docs/guides/community/jev-tutorial

#[derive(Debug, Serialize)]
struct NoulCriteria {
    #[serde(rename = "true")]
    yes: String,
    #[serde(rename = "false")]
    no: String,
}

#[derive(Debug, Serialize)]
struct NoulQuestion {
    #[serde(rename = "type")]
    kind: &'static str, // "noul"
    instructions: String,
    criteria: NoulCriteria,
}

#[derive(Debug, Serialize)]
struct DecisionsRequest {
    model: String,
    state: serde_json::Value,
    questions: std::collections::HashMap<String, NoulQuestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DecisionsResponse {
    #[serde(default)]
    answers: std::collections::HashMap<String, serde_json::Value>,
    #[serde(default)]
    usage: DecisionsUsage,
}

#[derive(Debug, Deserialize, Default)]
struct DecisionsUsage {
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[allow(dead_code)]
/// Extract "yes"/"no" + percentage robustly from free model text.
/// percentage = spam probability 0-100 (the prompt defines it that way).
fn parse_verdict_text(s: &str) -> Result<LlmVerdict, String> {
    let t = s.trim().trim_start_matches("```json").trim_start_matches("```")
        .trim_end_matches("```").trim();
    // 1) try strict JSON first
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
        let verdict_str = v.get("verdict").and_then(|x| x.as_str()).unwrap_or("")
            .to_lowercase();
        let bool_alt = v.get("verdict").and_then(|x| x.as_bool())
            .or_else(|| v.get("spam").and_then(|x| x.as_bool()));
        let pct = v.get("percentage").or_else(|| v.get("score")).or_else(|| v.get("confidence"))
            .and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.trim_end_matches('%').parse().ok())));
        let reason = v.get("reason").or_else(|| v.get("explanation"))
            .and_then(|x| x.as_str()).unwrap_or("").to_string();
        if !verdict_str.is_empty() || bool_alt.is_some() || pct.is_some() {
            let spam = bool_alt.unwrap_or_else(|| verdict_str.starts_with("yes") || verdict_str == "true" || verdict_str == "1");
            let mut score = pct.unwrap_or(if spam { 90.0 } else { 10.0 }) as f32;
            if score > 1.0 {
                score /= 100.0;
            }
            return Ok(LlmVerdict { spam, score: score.clamp(0.0, 1.0), reason: reason.chars().take(200).collect() });
        }
    }
    // 2) Fallback: first yes/no + first NN% value
    let low = t.to_lowercase();
    let has_yes = low.contains("yes");
    let has_no = low.contains("no");
    let mut pct: Option<f32> = None;
    let bytes = low.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
                j += 1;
            }
            let mut k = j;
            while k < bytes.len() && bytes[k] == b' ' {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b'%' {
                if let Ok(n) = low[i..j].parse::<f32>() {
                    pct = Some(n);
                    break;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    if has_yes || has_no || pct.is_some() {
        let spam = if has_yes && !has_no {
            true
        } else if has_no && !has_yes {
            false
        } else {
            pct.unwrap_or(50.0) >= 50.0
        };
        let score = pct.map(|p| (p / 100.0).clamp(0.0, 1.0)).unwrap_or(if spam { 0.85 } else { 0.15 });
        return Ok(LlmVerdict { spam, score, reason: t.chars().take(200).collect() });
    }
    Err(format!("unparseable verdict: {}", t.chars().take(120).collect::<String>()))
}

/// STATE for the Decisions API: all headers as an array (duplicates like Received
/// are preserved), plus envelope + body excerpt.
#[allow(dead_code)]
fn build_state_value(base: &LlmRequest) -> serde_json::Value {
    let headers: Vec<String> = base.headers.iter().map(|(n, v)| format!("{n}: {v}")).collect();
    serde_json::json!({
        "envelope_from": base.envelope_from,
        "envelope_rcpts": base.envelope_rcpts,
        "client_addr": base.client_addr,
        "helo": base.helo,
        "email-headers": headers,
        "body_excerpt": base.body_excerpt,
        "body_truncated": base.body_truncated,
    })
}

/// Noul yes-probability (0-1) -> verdict. Spam cut at 0.5, the rest is handled by
/// score_threshold + decision policy in the caller.
fn verdict_from_noul(p: f64) -> LlmVerdict {
    let p = (p as f32).clamp(0.0, 1.0);
    LlmVerdict { spam: p >= 0.5, score: p, reason: format!("noul={p:.3}") }
}

async fn query_openrouter(
    cfg: &AppConfig,
    ep: &ModelEndpoint,
    base: &LlmRequest,
) -> Result<(LlmVerdict, Option<f64>), String> {
    let mut questions = std::collections::HashMap::new();
    questions.insert("is_spam".to_string(), NoulQuestion {
        kind: "noul",
        instructions: cfg.question_instructions.clone(),
        criteria: NoulCriteria { yes: cfg.question_true.clone(), no: cfg.question_false.clone() },
    });
    let body = DecisionsRequest {
        model: ep.name.clone(),
        state: build_state_value(base),
        questions,
        session_id: if base.queue_id.is_empty() { None } else { Some(base.queue_id.clone()) },
    };
    let mut b = client().post(&cfg.openrouter_url).json(&body).timeout(cfg.llm_timeout);
    if let Some(key) = &cfg.llm_api_key {
        b = b.bearer_auth(key);
    }
    if let Some(r) = &cfg.openrouter_referer {
        b = b.header("HTTP-Referer", r);
    }
    if let Some(t) = &cfg.openrouter_title {
        b = b.header("X-Title", t);
    }
    let resp = b.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        // Unpack the OpenRouter error format {error:{message}}, otherwise a snippet.
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()).map(str::to_string))
            .unwrap_or_else(|| text.chars().take(200).collect::<String>());
        return Err(format!("decisions http {status}: {msg}"));
    }
    let dec = resp.json::<DecisionsResponse>().await.map_err(|e| e.to_string())?;
    let ans = dec.answers.get("is_spam").ok_or("decisions: missing is_spam answer")?;
    let p = ans.get("noul").and_then(|v| v.as_f64())
        .ok_or_else(|| format!("decisions: no noul value in {ans}"))?;
    Ok((verdict_from_noul(p), dec.usage.cost))
}

async fn query_legacy(
    cfg: &AppConfig,
    ep: &ModelEndpoint,
    base: &LlmRequest,
) -> Result<LlmVerdict, String> {
    let mut req = base.clone();
    req.model = ep.name.clone();
    let mut b = client().post(&ep.url).json(&req).timeout(cfg.llm_timeout);
    if let Some(key) = &cfg.llm_api_key {
        b = b.bearer_auth(key);
    }
    let resp = b.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("llm http {}", resp.status()));
    }
    resp.json::<LlmVerdict>().await.map_err(|e| e.to_string())
}

async fn query_one(
    cfg: &AppConfig,
    ep: &ModelEndpoint,
    base: &LlmRequest,
) -> ModelOutcome {
    let t0 = Instant::now();
    let res: Result<(LlmVerdict, Option<f64>), String> = if cfg.openrouter_mode {
        query_openrouter(cfg, ep, base).await
    } else {
        query_legacy(cfg, ep, base).await.map(|v| (v, None))
    };
    let latency_ms = t0.elapsed().as_millis() as u64;
    match res {
        Ok((v, cost)) => {
            let is_spam = v.spam || v.score >= cfg.score_threshold;
            ModelOutcome { model: ep.name.clone(), verdict: v, error: None, latency_ms, cost_usd: cost, is_spam }
        }
        Err(e) => ModelOutcome {
            model: ep.name.clone(),
            verdict: LlmVerdict::default(),
            error: Some(e),
            latency_ms,
            cost_usd: None,
            is_spam: false,
        },
    }
}

/// Query all models in parallel. Never returns an error:
/// a total outage is reported as empty/failed outcomes -> caller fails open.
async fn query_all(cfg: &AppConfig, base: &LlmRequest) -> Vec<ModelOutcome> {
    join_all(cfg.models.iter().map(|ep| query_one(cfg, ep, base))).await
}

fn combine(decision: Decision, outcomes: &[ModelOutcome]) -> bool {
    let ok: Vec<&ModelOutcome> = outcomes.iter().filter(|o| o.error.is_none()).collect();
    if ok.is_empty() {
        return false; // fail-open: no reject without a single successful answer
    }
    let spam = ok.iter().filter(|o| o.is_spam).count();
    match decision {
        Decision::Any => spam >= 1,
        Decision::All => spam == ok.len(),
        Decision::Majority => spam * 2 > ok.len(),
    }
}

fn verdict_tag(outcomes: &[ModelOutcome]) -> String {
    // compact: "m1=spam.92 m2=ham.10 m3=err" (watch the header length limit)
    let parts: Vec<String> = outcomes
        .iter()
        .map(|o| {
            if let Some(e) = &o.error {
                let short: String = e.chars().take(20).collect();
                format!("{}=err:{}", o.model, short)
            } else {
                let label = if o.is_spam { "spam" } else { "ham" };
                let reason: String = o.verdict.reason.chars().take(40).collect();
                format!("{}={}.{:.2} {}", o.model, label, o.verdict.score, reason).trim_end().to_string()
            }
        })
        .collect();
    let s = parts.join(" | ");
    s.chars().take(500).collect()
}

// ------------------------------------------------------------------ logging

/// Feed the dashboard counters. Cheap + infallible (lock poisoning is ignored).
fn record_stats(
    stats: &dashboard::Stats,
    queue_id: &str,
    sender: &str,
    spam: bool,
    whitelisted: bool,
    temperror: bool,
    outcomes: &[ModelOutcome],
) {
    let models: Vec<(String, bool, f32, u64, Option<f64>, bool)> = outcomes
        .iter()
        .map(|o| (o.model.clone(), o.is_spam, o.verdict.score, o.latency_ms, o.cost_usd, o.error.is_some()))
        .collect();
    stats.record(
        dashboard::RecentEntry {
            ts: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            queue_id: queue_id.to_string(),
            sender: sender.to_string(),
            spam,
            whitelisted,
            temperror,
            summary: if outcomes.is_empty() {
                if whitelisted { "whitelisted".into() } else { "temperror".into() }
            } else {
                verdict_tag(outcomes)
            },
            models: Vec::new(), // filled by Stats::record from `models`
        },
        &models,
    );
}

async fn append_eval_log(cfg: &AppConfig, line: &serde_json::Value) {
    use tokio::io::AsyncWriteExt;
    let s = serde_json::to_string(line).unwrap_or_default();
    // Parent anlegen (z.B. /var/lib/ai-classification-milter existiert frisch noch nicht).
    if let Some(parent) = cfg.eval_log.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                warn!(error = %e, "eval log parent create failed");
                return;
            }
        }
    }
    match tokio::fs::OpenOptions::new().create(true).append(true).open(&cfg.eval_log).await {
        Ok(mut f) => {
            if let Err(e) = f.write_all(format!("{s}\n").as_bytes()).await {
                warn!(error = %e, "eval log write failed");
            }
        }
        Err(e) => warn!(error = %e, "eval log open failed"),
    }
}

async fn log_mail_eml(
    cfg: &AppConfig,
    queue_id: &str,
    tag: &str,
    state: &MailState,
    outcomes: &[ModelOutcome],
    envelope_from: &str,
    envelope_rcpts: &[String],
) {
    if let Err(e) = tokio::fs::create_dir_all(&cfg.log_spam_dir).await {
        warn!(error = %e, "cannot create spam log dir");
        return;
    }
    let ts = Local::now().format("%Y%m%dT%H%M%S");
    let qid = if queue_id.is_empty() { "noqid".into() } else { queue_id.replace('/', "_") };
    let stem = format!("{ts}_{qid}_{tag}");
    let eml_path = cfg.log_spam_dir.join(format!("{stem}.eml"));
    let json_path = cfg.log_spam_dir.join(format!("{stem}.json"));

    let mut eml = String::with_capacity(state.body.len() + 4096);
    for (n, v) in &state.headers {
        eml.push_str(n);
        eml.push_str(": ");
        eml.push_str(v);
        eml.push_str("\r\n");
    }
    eml.push_str("\r\n");
    eml.push_str(&String::from_utf8_lossy(&state.body));

    let meta = serde_json::json!({
        "queue_id": queue_id,
        "verdict": tag, // "spam" | "ham" | "whitelisted" — used by the dashboard browser
        "outcomes": outcomes,
        "envelope_from": envelope_from,
        "envelope_rcpts": envelope_rcpts,
    });
    if let Err(e) = tokio::fs::write(&eml_path, eml.as_bytes()).await {
        warn!(error = %e, "eml write failed");
    } else {
        info!(path = %eml_path.display(), "mail logged");
    }
    if let Err(e) = tokio::fs::write(&json_path, serde_json::to_string_pretty(&meta).unwrap()).await {
        warn!(error = %e, "meta write failed");
    }
}

// ------------------------------------------------------------------ CLI mode
//
// `ai-classification-milter -i < mail.eml`: read mail from stdin (header unfolding included),
// query all models in parallel, print the result. No Postfix needed.

fn parse_mail(cfg: &AppConfig, raw: &str) -> MailState {
    let mut st = MailState::default();
    let (head, body) = match raw.split_once("\r\n\r\n") {
        Some(x) => x,
        None => match raw.split_once("\n\n") {
            Some(x) => x,
            None => (raw, ""),
        },
    };
    let mut name: Option<String> = None;
    let mut value = String::new();
    for line in head.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(' ') || line.starts_with('\t') {
            // folded header: append
            if name.is_some() {
                value.push(' ');
                value.push_str(line.trim());
            }
        } else {
            if let Some(n) = name.take() {
                st.push_header(cfg, &n, &value);
                value.clear();
            }
            if let Some((n, v)) = line.split_once(':') {
                name = Some(n.trim().to_string());
                value.push_str(v.trim());
            }
        }
    }
    if let Some(n) = name.take() {
        st.push_header(cfg, &n, &value);
    }
    st.push_body(cfg, body.as_bytes());
    st
}

async fn run_cli(cfg: &AppConfig, json_out: bool) -> std::io::Result<()> {
    use tokio::io::AsyncReadExt;
    let mut raw = String::new();
    tokio::io::stdin().read_to_string(&mut raw).await?;
    if raw.trim().is_empty() {
        eprintln!("empty input: paste mail on stdin, then Ctrl+D");
        return Ok(());
    }
    let state = parse_mail(cfg, &raw);
    // Whitelist: don't even send to the models, auto-accept.
    let envelope_from = state
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("from"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    if is_sender_whitelisted(cfg, "", &state.headers) {
        if json_out {
            println!(
                "{}",
                serde_json::json!({ "spam": false, "whitelisted": true, "envelope_from": envelope_from, "outcomes": [] })
            );
            return Ok(());
        }
        println!("WHITELISTED (sender domain): {} -> ham (no LLM call)", envelope_from);
        return Ok(());
    }
    let body_text = String::from_utf8_lossy(&state.body);
    let base = LlmRequest {
        model: String::new(),
        queue_id: "cli".into(),
        client_addr: String::new(),
        helo: String::new(),
        envelope_from: String::new(),
        envelope_rcpts: Vec::new(),
        headers: state.headers.clone(),
        body_excerpt: body_text.chars().take(cfg.llm_body_chars).collect(),
        body_truncated: state.body_truncated,
    };

    let outcomes = query_all(cfg, &base).await;
    let spam = combine(cfg.decision, &outcomes);

    if json_out {
        println!("{}", serde_json::json!({ "spam": spam, "outcomes": outcomes }));
        return Ok(());
    }
    println!("{:<38} {:<7} {:<7} {:<9} {}", "model", "verdict", "score", "latency", "cost/error");
    for o in &outcomes {
        let (verdict, score) = match &o.error {
            Some(e) => (format!("ERR"), e.chars().take(40).collect::<String>()),
            None => (
                if o.is_spam { "SPAM" } else { "ham" }.to_string(),
                format!("{:.3}", o.verdict.score),
            ),
        };
        let cost = o.cost_usd.map(|c| format!("${c:.5}")).unwrap_or_else(|| "-".into());
        println!("{:<38} {:<7} {:<7} {:<9} {}", o.model, verdict, score, format!("{}ms", o.latency_ms), cost);
    }
    println!("COMBINED ({:?}): {}", cfg.decision, if spam { "SPAM" } else { "ham" });
    Ok(())
}

fn print_usage() {
    println!("usage: ai-classification-milter [-i|--stdin] [--json] [-c|--config <path>] [-h|--help]");
    println!("  no args: milter daemon (Postfix).");
    println!("  -i:        read mail from stdin (paste + Ctrl+D), query all models.");
    println!("  --json:    combinable with -i, JSON instead of table.");
}

// ------------------------------------------------------------------ main

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return Ok(());
    }
    let mut stdin_mode = false;
    let mut json_out = false;
    let mut i = 0;
    while i < raw_args.len() {
        match raw_args[i].as_str() {
            "-i" | "--stdin" => stdin_mode = true,
            "--json" => json_out = true,
            "-c" | "--config" => {
                i += 1;
                if let Some(p) = raw_args.get(i) {
                    std::env::set_var("MILTER_CONFIG", p);
                }
            }
            _ => {}
        }
        i += 1;
    }

    let cfg = Arc::new(AppConfig::load());
    if stdin_mode {
        return run_cli(&cfg, json_out).await;
    }
    let stats = dashboard::Stats::new();
    info!(
        listen = %cfg.listen,
        models = ?cfg.models.iter().map(|m| &m.name).collect::<Vec<_>>(),
        learn_mode = cfg.learn_mode,
        whitelist_sender_domains = ?cfg.whitelist_sender_domains,
        dashboard_enabled = cfg.dashboard_enabled,
        dashboard_listen = %cfg.dashboard_listen,
        "ai-classification-milter starting"
    );

    // Optional dashboard (own socket, default off). Runs as background task;
    // failure to bind must not kill the milter — just log it.
    if cfg.dashboard_enabled {
        let dcfg = cfg.clone();
        let dstats = stats.clone();
        tokio::spawn(async move {
            let opts = dashboard::DashboardOpts {
                stats: dstats,
                spam_dir: dcfg.log_spam_dir.clone(),
                token: dcfg.dashboard_token.clone(),
                scramble_senders: dcfg.dashboard_scramble_senders,
            };
            if let Err(e) = dashboard::serve(dcfg.dashboard_listen.clone(), opts).await {
                warn!(error = %e, "dashboard failed");
            }
        });
    }

    let cbs = Callbacks::<MailState>::new()
        .on_mail(|ctx: &mut Context<MailState>, args: Vec<CString>| {
            Box::pin(async move {
                let sender = args.first().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
                ctx.data = Some(MailState { sender, ..Default::default() });
                Status::Continue
            })
        })
        .on_rcpt(|ctx: &mut Context<MailState>, args: Vec<CString>| {
            Box::pin(async move {
                if let Some(st) = ctx.data.as_mut() {
                    if let Some(rcpt) = args.first() {
                        st.recipients.push(rcpt.to_string_lossy().into_owned());
                    }
                }
                Status::Continue
            })
        })
        .on_header({
            let cfg = cfg.clone();
            move |ctx: &mut Context<MailState>, name: CString, value: CString| {
                let cfg = cfg.clone();
                Box::pin(async move {
                    if let Some(st) = ctx.data.as_mut() {
                        st.push_header(&cfg, &name.to_string_lossy(), &value.to_string_lossy());
                    }
                    Status::Continue
                })
            }
        })
        .on_body({
            let cfg = cfg.clone();
            move |ctx: &mut Context<MailState>, chunk: Bytes| {
                let cfg = cfg.clone();
                Box::pin(async move {
                    if let Some(st) = ctx.data.as_mut() {
                        st.push_body(&cfg, &chunk);
                    }
                    Status::Continue
                })
            }
        })
        .on_abort(|ctx: &mut Context<MailState>| {
            Box::pin(async move {
                ctx.data = None;
                Status::Continue
            })
        })
        .on_eom({
            let cfg = cfg.clone();
            let stats = stats.clone();
            move |ctx: &mut EomContext<MailState>| {
                let cfg = cfg.clone();
                let stats = stats.clone();
                Box::pin(async move {
                    // Everything below is fail-open: no path may panic/reject
                    // without a successful model answer.
                    let state = ctx.data.take().unwrap_or_default();
                    let queue_id = get_macro(&ctx.macros, b"i\0");
                    let client_addr = get_macro(&ctx.macros, b"{client_addr}\0");
                    let helo = get_macro(&ctx.macros, b"{client_name}\0");

                    // Whitelisted sender domain: no model call, accept immediately + tag.
                    if is_sender_whitelisted(&cfg, &state.sender, &state.headers) {
                        info!(queue_id = %queue_id, sender = %state.sender, "whitelisted sender domain, skip LLM");
                        record_stats(&stats, &queue_id, &state.sender, false, true, false, &[]);
                        append_eval_log(&cfg, &serde_json::json!({
                            "ts": Local::now().to_rfc3339(),
                            "queue_id": queue_id,
                            "envelope_from": state.sender,
                            "envelope_rcpts": state.recipients,
                            "spam": false,
                            "whitelisted": true,
                        }))
                        .await;
                        if cfg.log_all_mails {
                            log_mail_eml(&cfg, &queue_id, "whitelisted",
                                &state, &[], &state.sender, &state.recipients).await;
                        }
                        let _ = ctx.actions.add_header("X-LLM-Verdict", "whitelisted").await;
                        return Status::Continue;
                    }

                    let body_text = String::from_utf8_lossy(&state.body);
                    let excerpt: String = body_text.chars().take(cfg.llm_body_chars).collect();
                    let base = LlmRequest {
                        model: String::new(), // set per model
                        queue_id: queue_id.clone(),
                        client_addr,
                        helo,
                        envelope_from: state.sender.clone(),
                        envelope_rcpts: state.recipients.clone(),
                        headers: state.headers.clone(),
                        body_excerpt: excerpt,
                        body_truncated: state.body_truncated || state.body.len() >= cfg.body_max_bytes,
                    };

                    let outcomes = query_all(&cfg, &base).await;
                    let ok_count = outcomes.iter().filter(|o| o.error.is_none()).count();
                    let spam = combine(cfg.decision, &outcomes);
                    info!(queue_id = %queue_id, spam = spam, ok = ok_count, total = outcomes.len(), "verdicts");
                    record_stats(&stats, &queue_id, &state.sender, spam, false, ok_count == 0, &outcomes);

                    // Eval log: one JSON line per mail (cheap, no body).
                    append_eval_log(&cfg, &serde_json::json!({
                        "ts": Local::now().to_rfc3339(),
                        "queue_id": queue_id,
                        "envelope_from": state.sender,
                        "envelope_rcpts": state.recipients,
                        "decision": format!("{:?}", cfg.decision),
                        "spam": spam,
                        "outcomes": outcomes,
                    }))
                    .await;

                    // Full log: always for spam, for ham only with LOG_ALL_MAILS=1 (eval).
                    if spam || cfg.log_all_mails {
                        log_mail_eml(&cfg, &queue_id, if spam { "spam" } else { "ham" },
                            &state, &outcomes, &base.envelope_from, &base.envelope_rcpts).await;
                    }

                    if ok_count == 0 {
                        warn!(queue_id = %queue_id, "all models failed, fail-open");
                        let _ = ctx.actions.add_header("X-LLM-Verdict", "temperror").await;
                        return Status::Continue;
                    }

                    if spam {
                        if cfg.learn_mode {
                            let _ = ctx.actions.add_header("X-LLM-Verdict", verdict_tag(&outcomes)).await;
                            return Status::Continue;
                        }
                        let _ = ctx.reply.set_error_reply(
                            "550",
                            Some("5.7.1"),
                            ["Spam rejected by policy"],
                        );
                        return Status::Reject;
                    }

                    let _ = ctx.actions.add_header("X-LLM-Verdict", verdict_tag(&outcomes)).await;
                    Status::Continue
                })
            }
        });

    let mut milter_cfg = Config::default();
    milter_cfg.actions = Actions::ADD_HEADER;

    let listener = TcpListener::bind(&cfg.listen).await?;
    let shutdown = async { tokio::signal::ctrl_c().await.ok(); };
    indymilter::run(listener, cbs, milter_cfg, shutdown).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_request_matches_docs_schema() {
        let mut questions = std::collections::HashMap::new();
        questions.insert("is_spam".to_string(), NoulQuestion {
            kind: "noul",
            instructions: DEFAULT_QUESTION.into(),
            criteria: NoulCriteria { yes: "y".into(), no: "n".into() },
        });
        let req = DecisionsRequest {
            model: "typesafe/jev-1.13".into(),
            state: serde_json::json!({"headers": ["Subject: x"]}),
            questions,
            session_id: Some("ABC123".into()),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["model"], "typesafe/jev-1.13");
        assert_eq!(v["questions"]["is_spam"]["type"], "noul");
        assert_eq!(v["questions"]["is_spam"]["criteria"]["true"], "y");
        assert_eq!(v["questions"]["is_spam"]["criteria"]["false"], "n");
        assert_eq!(v["session_id"], "ABC123");
        // Noul answer: yes probability
        let ans: serde_json::Value = serde_json::json!({"type": "noul", "noul": 0.96});
        assert!((ans["noul"].as_f64().unwrap() - 0.96).abs() < 1e-9);
    }

    fn test_config() -> AppConfig {
        AppConfig {
            listen: "127.0.0.1:0".into(),
            models: vec![],
            openrouter_url: DEFAULT_OPENROUTER_URL.into(),
            openrouter_mode: true,
            openrouter_referer: None,
            openrouter_title: None,
            question_instructions: DEFAULT_QUESTION.into(),
            question_true: DEFAULT_CRITERIA_TRUE.into(),
            question_false: DEFAULT_CRITERIA_FALSE.into(),
            llm_api_key: None,
            llm_timeout: Duration::from_secs(5),
            score_threshold: 0.85,
            decision: Decision::Any,
            header_max_count: 3,
            header_max_value_chars: 5,
            body_max_bytes: 10,
            llm_body_chars: 4000,
            log_spam_dir: PathBuf::from("/tmp"),
            eval_log: PathBuf::from("/tmp/eval.jsonl"),
            log_all_mails: false,
            learn_mode: true,
            whitelist_sender_domains: vec!["example.com".into(), "dept.example.com".into()],
            dashboard_enabled: false,
            dashboard_listen: "127.0.0.1:0".into(),
            dashboard_token: None,
            dashboard_scramble_senders: false,
        }
    }

    fn outcome(model: &str, spam: bool, score: f32) -> ModelOutcome {
        ModelOutcome {
            model: model.into(),
            verdict: LlmVerdict { spam, score, reason: String::new() },
            error: None,
            latency_ms: 1,
            cost_usd: None,
            is_spam: spam || score >= 0.85,
        }
    }

    fn failed(model: &str) -> ModelOutcome {
        ModelOutcome {
            model: model.into(),
            verdict: LlmVerdict::default(),
            error: Some("timeout".into()),
            latency_ms: 5000,
            cost_usd: None,
            is_spam: false,
        }
    }

    #[test]
    fn verdict_from_noul_boundaries() {
        assert!(!verdict_from_noul(0.0).spam);
        assert!(!verdict_from_noul(0.499).spam);
        assert!(verdict_from_noul(0.5).spam);
        assert!(verdict_from_noul(0.96).spam);
        assert_eq!(verdict_from_noul(0.96).score, 0.96_f32);
        // clamping
        assert_eq!(verdict_from_noul(-0.2).score, 0.0);
        assert_eq!(verdict_from_noul(1.5).score, 1.0);
        assert!(verdict_from_noul(1.5).spam);
    }

    #[test]
    fn combine_policies() {
        let mixed = vec![outcome("a", true, 0.9), outcome("b", false, 0.1), outcome("c", false, 0.2)];
        assert!(combine(Decision::Any, &mixed));
        assert!(!combine(Decision::Majority, &mixed));
        assert!(!combine(Decision::All, &mixed));

        let two_spam = vec![outcome("a", true, 0.9), outcome("b", true, 0.8), outcome("c", false, 0.1)];
        assert!(combine(Decision::Any, &two_spam));
        assert!(combine(Decision::Majority, &two_spam));
        assert!(!combine(Decision::All, &two_spam));

        // score above threshold also counts without the spam flag
        let by_score = vec![outcome("a", false, 0.9)];
        assert!(combine(Decision::Any, &by_score));
    }

    #[test]
    fn combine_fail_open() {
        // no successful answer at all -> never spam (fail-open)
        assert!(!combine(Decision::Any, &[failed("a"), failed("b")]));
        assert!(!combine(Decision::Majority, &[failed("a")]));
        assert!(!combine(Decision::All, &[]));
        // errors are excluded from the majority vote
        let with_err = vec![outcome("a", true, 0.9), failed("b"), failed("c")];
        assert!(combine(Decision::Majority, &with_err));
        assert!(combine(Decision::All, &with_err));
    }

    #[test]
    fn verdict_tag_format_and_cap() {
        let outcomes = vec![outcome("a", true, 0.92), outcome("b", false, 0.1), failed("c")];
        let tag = verdict_tag(&outcomes);
        assert!(tag.contains("a=spam.0.92"));
        assert!(tag.contains("b=ham.0.10"));
        assert!(tag.contains("c=err:timeout"));
        // 500-char cap
        let many: Vec<ModelOutcome> = (0..50).map(|i| outcome(&format!("model-{i:02}"), true, 0.99)).collect();
        assert!(verdict_tag(&many).chars().count() <= 500);
    }

    #[test]
    fn parse_mail_headers_and_body() {
        let cfg = test_config();
        let raw = "From: a@x\r\nSubject: hi\r\nReceived: from mx\r\n\twith ESMTP id 1\r\n\r\nhello body";
        let st = parse_mail(&cfg, raw);
        // value cap of 5 chars applies
        assert_eq!(st.headers[0], ("From".into(), "a@x".into()));
        assert_eq!(st.headers[1].0, "Subject");
        // folded Received header was merged (truncated to 5 chars)
        assert_eq!(st.headers[2].0, "Received");
        assert!(st.headers[2].1.starts_with("from"));
        assert_eq!(String::from_utf8_lossy(&st.body), "hello body");
        assert!(!st.body_truncated);
    }

    #[test]
    fn parse_mail_lf_only_and_no_body() {
        let cfg = test_config();
        let st = parse_mail(&cfg, "Subject: x\nX-A: 1\n");
        assert_eq!(st.headers.len(), 2);
        assert!(st.body.is_empty());
    }

    #[test]
    fn parse_mail_caps() {
        let cfg = test_config(); // max 3 header, 10 body bytes
        let raw = "A: 1\nB: 2\nC: 3\nD: 4\n\n0123456789abcdef";
        let st = parse_mail(&cfg, raw);
        assert_eq!(st.headers.len(), 3); // D is dropped
        assert_eq!(st.body.len(), 10);
        assert!(st.body_truncated);
    }

    #[test]
    fn build_state_uses_email_headers_key() {
        let base = LlmRequest {
            model: String::new(),
            queue_id: "q".into(),
            client_addr: String::new(),
            helo: String::new(),
            envelope_from: String::new(),
            envelope_rcpts: vec![],
            headers: vec![
                ("Received".into(), "from a".into()),
                ("Received".into(), "from b".into()),
            ],
            body_excerpt: String::new(),
            body_truncated: false,
        };
        let s = build_state_value(&base);
        // duplicates are preserved as an array
        assert_eq!(s["email-headers"], serde_json::json!(["Received: from a", "Received: from b"]));
        assert!(s.get("headers").is_none());
    }

    #[test]
    fn legacy_verdict_parser() {
        let v = parse_verdict_text(r#"{"verdict":"yes","percentage":87,"reason":"bulk"}"#).unwrap();
        assert!(v.spam && (v.score - 0.87).abs() < 1e-6);
        let v = parse_verdict_text("No, 5%").unwrap();
        assert!(!v.spam);
        let v = parse_verdict_text("```json\n{\"spam\":true,\"score\":0.7}\n```").unwrap();
        assert!(v.spam);
        assert!(parse_verdict_text("hello world").is_err());
    }

    #[test]
    fn file_config_toml_parse() {
        let fc: FileConfig = toml::from_str(
            r#"
            listen = "127.0.0.1:8899"
            [openrouter]
            models = ["a/b", "c/d"]
            [openrouter.question]
            instructions = "Q?"
            [limits]
            llm_timeout_secs = 7
            decision = "majority"
            [logging]
            learn_mode = false
            "#,
        )
        .unwrap();
        assert_eq!(fc.listen.unwrap(), "127.0.0.1:8899");
        assert_eq!(fc.openrouter.unwrap().models.unwrap(), vec!["a/b", "c/d"]);
        assert_eq!(fc.limits.as_ref().unwrap().decision.as_deref(), Some("majority"));
        assert_eq!(fc.logging.unwrap().learn_mode, Some(false));
    }

    #[test]
    fn file_config_dashboard_parse() {
        let fc: FileConfig = toml::from_str(
            r#"
            [dashboard]
            enabled = true
            listen = "0.0.0.0:8892"
            token = "s3cret"
            scramble_senders = true
            "#,
        )
        .unwrap();
        let d = fc.dashboard.unwrap();
        assert_eq!(d.enabled, Some(true));
        assert_eq!(d.listen.as_deref(), Some("0.0.0.0:8892"));
        assert_eq!(d.token.as_deref(), Some("s3cret"));
        assert_eq!(d.scramble_senders, Some(true));
        // defaults: off
        let fc2: FileConfig = toml::from_str("").unwrap();
        assert!(fc2.dashboard.is_none());
    }

    #[test]
    fn openrouter_endpoints_precedence() {
        let no_env = |_: &str| None;
        let or = OpenRouterSection::default();
        // Defaults: 5 models
        let (v, mode) = openrouter_endpoints(&no_env, &or);
        assert!(mode && v.len() == 5);
        // Env beats TOML
        let env = |k: &str| if k == "OPENROUTER_MODELS" { Some("x/y".into()) } else { None };
        let or = OpenRouterSection { models: Some(vec!["toml/m".into()]), ..Default::default() };
        let (v, _) = openrouter_endpoints(&env, &or);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "x/y");
        // TOML applies without env
        let (v, _) = openrouter_endpoints(&no_env, &or);
        assert_eq!(v[0].name, "toml/m");
    }

    #[test]
    fn sender_domain_extraction() {
        assert_eq!(sender_domain("<user@example.com>").as_deref(), Some("example.com"));
        assert_eq!(sender_domain("user@EXAMPLE.COM").as_deref(), Some("example.com"));
        assert_eq!(sender_domain("\"Doe, John\" <john@sub.example.com>").as_deref(), Some("sub.example.com"));
        assert_eq!(sender_domain("  <USER@Example.Com.>  ").as_deref(), Some("example.com"));
        assert_eq!(sender_domain("<>"), None);
        assert_eq!(sender_domain(""), None);
        assert_eq!(sender_domain("not-an-addr"), None);
        assert_eq!(sender_domain("user@localhost"), None);
    }

    #[test]
    fn whitelist_exact_and_subdomain() {
        let cfg = test_config(); // example.com, dept.example.com
        assert!(is_sender_whitelisted(&cfg, "<a@example.com>", &[]));
        assert!(is_sender_whitelisted(&cfg, "a@dept.example.com", &[]));
        assert!(is_sender_whitelisted(&cfg, "<a@MAIL.example.com>", &[]));
        assert!(is_sender_whitelisted(&cfg, "<a@sub.mail.dept.example.com>", &[]));
        // lookalike domains must not match
        assert!(!is_sender_whitelisted(&cfg, "<a@notexample.com>", &[]));
        assert!(!is_sender_whitelisted(&cfg, "<a@example.com.attacker.com>", &[]));
        assert!(!is_sender_whitelisted(&cfg, "<a@gmail.com>", &[]));
        assert!(!is_sender_whitelisted(&cfg, "<>", &[]));
    }

    #[test]
    fn whitelist_from_header_fallback_only_without_envelope() {
        let cfg = test_config();
        let hdrs = vec![("From".to_string(), "boss@example.com".to_string())];
        // CLI fallback: empty envelope -> From header counts
        assert!(is_sender_whitelisted(&cfg, "", &hdrs));
        // With an envelope set, only the envelope counts (From is ignored)
        assert!(!is_sender_whitelisted(&cfg, "<spam@evil.com>", &hdrs));
        // Empty whitelist -> never matches
        let mut empty = test_config();
        empty.whitelist_sender_domains.clear();
        assert!(!is_sender_whitelisted(&empty, "<a@example.com>", &[]));
    }

    #[test]
    fn file_config_whitelist_parse() {
        let fc: FileConfig = toml::from_str(
            r#"
            [whitelist]
            sender_domains = ["example.com", "Mail.Example.COM", " .sub.example.com. "]
            "#,
        )
        .unwrap();
        let domains = fc.whitelist.unwrap().sender_domains.unwrap();
        assert_eq!(domains, vec!["example.com", "Mail.Example.COM", " .sub.example.com. "]);
        // normalize_domain cleans up
        let norm: Vec<String> = domains.iter().map(|s| normalize_domain(s)).collect();
        assert_eq!(norm, vec!["example.com", "mail.example.com", "sub.example.com"]);
    }
}
