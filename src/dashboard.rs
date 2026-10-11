//! Minimal dashboard: stats + quarantine browser, zero extra dependencies.
//!
//! Runs as a tiny hand-rolled HTTP server on a separate Tokio TcpListener so the
//! milter itself stays dependency-free. Only GET is supported.
//!
//! Routes:
//!   GET /                  HTML overview (stats + recent + link to /mails)
//!   GET /mails             HTML list of quarantined *.eml (+ ?q= search)
//!   GET /mails/<stem>      HTML detail (meta + .eml preview)
//!   GET /mails/<stem>/raw  raw .eml download (text/plain)
//!   GET /api/stats         JSON counters (since process start)
//!   GET /api/mails         JSON list (?q=&limit=&offset=)
//!   GET /api/mails/<stem>  JSON {meta, eml_preview}
//!
//! Auth: if a token is configured, every request needs
//! `Authorization: Bearer <token>` or `?token=<token>`. Otherwise open
//! (default bind is 127.0.0.1 — keep it that way unless behind a reverse proxy,
//! the quarantine contains PII).

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

/// Shared counters, updated from the milter EOM path. Everything since process start.
#[derive(Debug, Default)]
pub struct Stats {
    inner: RwLock<StatsInner>,
    pub started_unix: u64,
}

#[derive(Debug, Default)]
struct StatsInner {
    total: u64,
    spam: u64,
    ham: u64,
    whitelisted: u64,
    temperror: u64,
    per_model: HashMap<String, ModelStats>,
    recent: VecDeque<RecentEntry>,
}

#[derive(Debug, Default, Clone)]
pub struct ModelStats {
    pub ok: u64,
    pub spam: u64,
    pub errors: u64,
    pub score_sum: f64,
    pub latency_ms_sum: u64,
    pub cost_usd_sum: f64,
}

#[derive(Debug, Clone)]
pub struct RecentEntry {
    pub ts: String,
    pub queue_id: String,
    pub sender: String,
    pub spam: bool,
    pub whitelisted: bool,
    pub temperror: bool,
    pub summary: String,
    pub models: Vec<ModelChip>,
}

/// Flat outcome row handed to `Stats::record`:
/// (model, is_spam, score, latency_ms, cost_usd, failed).
pub type ModelRow = (String, bool, f32, u64, Option<f64>, bool);

/// One model result for chip rendering (overview + quarantine list).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelChip {
    pub name: String, // shortened, e.g. "jev-1.13"
    pub spam: bool,
    pub err: bool,
    pub score: f32,
}

/// "org/model-name…" -> "model-name…" (max 16 chars) for compact chips.
fn short_model(name: &str) -> String {
    name.split('/')
        .next_back()
        .unwrap_or(name)
        .chars()
        .take(16)
        .collect()
}

fn chips_html(models: &[ModelChip]) -> String {
    let mut h = String::with_capacity(models.len() * 48);
    for m in models {
        let (cls, label) = if m.err {
            ("e", "err".to_string())
        } else if m.spam {
            ("s", format!("SPAM {:.2}", m.score))
        } else {
            ("h", format!("ham {:.2}", m.score))
        };
        h.push_str(&format!(
            "<span class=\"chip {cls}\">{}</span>",
            html_escape(&format!("{} · {label}", m.name))
        ));
    }
    h
}

impl Stats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(StatsInner::default()),
            started_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })
    }

    /// Outcome row as recorded: see `ModelRow`.
    pub fn record(&self, mut entry: RecentEntry, models: &[ModelRow]) {
        let mut g = match self.inner.write() {
            Ok(g) => g,
            Err(_) => return,
        };
        g.total += 1;
        if entry.whitelisted {
            g.whitelisted += 1;
            g.ham += 1;
        } else if entry.temperror {
            g.temperror += 1;
            g.ham += 1; // fail-open: accepted
        } else if entry.spam {
            g.spam += 1;
        } else {
            g.ham += 1;
        }
        for (name, is_spam, score, latency_ms, cost, failed) in models {
            entry.models.push(ModelChip {
                name: short_model(name),
                spam: *is_spam,
                err: *failed,
                score: *score,
            });
            let m = g.per_model.entry(name.clone()).or_default();
            if *failed {
                m.errors += 1;
            } else {
                m.ok += 1;
                if *is_spam {
                    m.spam += 1;
                }
                m.score_sum += f64::from(*score);
                m.latency_ms_sum += latency_ms;
                if let Some(c) = cost {
                    m.cost_usd_sum += c;
                }
            }
        }
        g.recent.push_front(entry);
        while g.recent.len() > 100 {
            g.recent.pop_back();
        }
    }

    pub fn snapshot_json(&self, quarantine_count: usize) -> serde_json::Value {
        let g = match self.inner.read() {
            Ok(g) => g,
            Err(_) => return serde_json::json!({"error": "stats locked"}),
        };
        let per_model: serde_json::Value = g
            .per_model
            .iter()
            .map(|(name, m)| {
                let avg_score = if m.ok > 0 {
                    m.score_sum / m.ok as f64
                } else {
                    0.0
                };
                let avg_lat = if m.ok > 0 {
                    m.latency_ms_sum as f64 / m.ok as f64
                } else {
                    0.0
                };
                (
                    name.clone(),
                    serde_json::json!({
                        "ok": m.ok,
                        "spam": m.spam,
                        "errors": m.errors,
                        "avg_score": (avg_score * 1000.0).round() / 1000.0,
                        "avg_latency_ms": avg_lat.round() as u64,
                        "cost_usd": (m.cost_usd_sum * 100_000.0).round() / 100_000.0,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into();
        serde_json::json!({
            "started_unix": self.started_unix,
            "total": g.total,
            "spam": g.spam,
            "ham": g.ham,
            "whitelisted": g.whitelisted,
            "temperror": g.temperror,
            "quarantine_files": quarantine_count,
            "per_model": per_model,
            "recent": g.recent.iter().map(|r| serde_json::json!({
                "ts": r.ts, "queue_id": r.queue_id, "sender": r.sender,
                "spam": r.spam, "whitelisted": r.whitelisted, "temperror": r.temperror,
                "summary": r.summary, "models": r.models,
            })).collect::<Vec<_>>(),
        })
    }

    fn recent_html(&self, scr: bool) -> String {
        let g = match self.inner.read() {
            Ok(g) => g,
            Err(_) => return String::new(),
        };
        let mut h = String::from(
            "<table><tr><th>Time</th><th>Queue</th><th>Sender</th><th>Verdict</th><th>Models</th></tr><tbody id=\"m-recent\">",
        );
        if g.recent.is_empty() {
            h.push_str("<tr><td colspan=5><i>No mails since start.</i></td></tr>");
        }
        for r in g.recent.iter().take(20) {
            let detail = if r.models.is_empty() {
                html_escape(&r.summary)
            } else {
                chips_html(&r.models)
            };
            h.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td{}>{}</td><td>{}</td><td>{}</td></tr>",
                html_escape(&r.ts),
                html_escape(&r.queue_id),
                if scr { " class=\"scr\"" } else { "" },
                html_escape(&r.sender),
                verdict_badge(r.whitelisted, r.temperror, r.spam),
                detail
            ));
        }
        h.push_str("</tbody></table>");
        h
    }

    fn overview_cards(&self, quarantine_count: usize) -> String {
        let (total, spam, ham, wl, terr) = match self.inner.read() {
            Ok(g) => (g.total, g.spam, g.ham, g.whitelisted, g.temperror),
            Err(_) => (0, 0, 0, 0, 0),
        };
        format!(
            "<div class=status><span id=\"m-dot\" class=\"dot on\">●</span><span id=\"m-updated\">live</span></div>\
            <div class=cards>\
            <div class=card><b id=\"c-total\">{total}</b><span>total</span></div>\
            <div class=\"card hot\"><b id=\"c-spam\">{spam}</b><span>spam</span></div>\
            <div class=card><b id=\"c-ham\">{ham}</b><span>ham</span></div>\
            <div class=card><b id=\"c-wl\">{wl}</b><span>whitelisted</span></div>\
            <div class=card><b id=\"c-terr\">{terr}</b><span>temperror</span></div>\
            <div class=card><b id=\"c-q\">{quarantine_count}</b><span>quarantined files</span></div>\
            </div>"
        )
    }

    fn per_model_html(&self) -> String {
        let rows: Vec<(String, ModelStats)> = match self.inner.read() {
            Ok(g) => g
                .per_model
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            Err(_) => Vec::new(),
        };
        let mut h = String::from(
            "<h2>Models</h2><table><tr><th>Model</th><th>ok</th><th>spam</th><th>err</th><th>avg score</th><th>avg latency</th><th>cost $</th></tr><tbody id=\"m-models\">",
        );
        if rows.is_empty() {
            h.push_str("<tr><td colspan=7><i>no data</i></td></tr>");
        }
        let mut rows = rows;
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, m) in rows {
            let avg_s = if m.ok > 0 {
                m.score_sum / m.ok as f64
            } else {
                0.0
            };
            let avg_l = if m.ok > 0 {
                m.latency_ms_sum as f64 / m.ok as f64
            } else {
                0.0
            };
            h.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{:.3}</td><td>{:.0}ms</td><td>{:.5}</td></tr>",
                html_escape(&name),
                m.ok,
                m.spam,
                m.errors,
                avg_s,
                avg_l,
                m.cost_usd_sum
            ));
        }
        h.push_str("</tbody></table>");
        h
    }
}

// ------------------------------------------------------------ quarantine scan

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QuarantineMail {
    pub stem: String,
    pub mtime_unix: u64,
    pub size: u64,
    pub sender: String,
    pub spam: Option<bool>,
    pub verdict: Option<String>, // "spam" | "ham" | "whitelisted" (sidecar), None for legacy files
    pub summary: String,
    pub models: Vec<ModelChip>,
}

pub fn quarantine_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter(is_eml).count())
        .unwrap_or(0)
}

fn is_eml(e: &std::io::Result<std::fs::DirEntry>) -> bool {
    e.as_ref()
        .map(|e| {
            e.path()
                .extension()
                .map(|x| x.eq_ignore_ascii_case("eml"))
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

// ------------------------------------------------------------------ release
//
// A quarantined mail that turns out to be a false positive has to go out after
// all. A milter cannot inject mail (the protocol has no such verb and the queue
// belongs to Postfix), so the dashboard hands the message back to the MTA via
// `sendmail` (pickup). Pickup does not pass milters, so the message cannot be
// filtered a second time — no loop, no allow-list needed.
//
// Safety: the message goes in over stdin and every argument is passed as a
// separate exec argument, so no shell is involved and a crafted envelope sender
// cannot inject options or extra headers.

/// Path to the local MTA delivery command. Override with `MILTER_SENDMAIL_CMD`
/// (absolute path, no arguments).
const DEFAULT_SENDMAIL: &str = "/usr/sbin/sendmail";

fn sendmail_cmd() -> String {
    std::env::var("MILTER_SENDMAIL_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SENDMAIL.to_string())
}

/// Reject anything that could smuggle an option or a second header into the
/// `sendmail` invocation. Returns None for a usable address.
fn sanitize_envelope(addr: &str) -> Option<String> {
    let a = addr.trim();
    if a.is_empty() || a.contains(['\n', '\r', '\0', ' ', '\t']) {
        return None;
    }
    // Must look like an address; anything else is refused outright.
    if !a.contains('@') {
        return None;
    }
    Some(a.to_string())
}

#[derive(Debug)]
pub enum ReleaseError {
    /// Bad input from the request (stem traversal, hostile envelope, ...).
    Invalid(String),
    /// The MTA refused or is not installed.
    Delivery(String),
}

/// Re-inject a quarantined mail. Returns the sender used.
pub fn release_mail(
    dir: &Path,
    stem: &str,
    eval_log: Option<&Path>,
) -> Result<String, ReleaseError> {
    // read_mail already rejects traversal; reuse it as the single gate.
    let (meta, eml) = read_mail(dir, stem)
        .ok_or_else(|| ReleaseError::Invalid(format!("unknown mail: {stem}")))?;
    let meta_v: serde_json::Value = serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null);
    let raw_from = meta_v
        .get("envelope_from")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let from = match sanitize_envelope(&raw_from) {
        Some(f) => f,
        None => {
            // Empty envelope (bounces) cannot be re-injected safely.
            return Err(ReleaseError::Invalid(format!(
                "unusable envelope sender: {raw_from:?}"
            )));
        }
    };

    // Stamp the release so the trail survives on the delivered mail.
    let stamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
    let note =
        format!("X-AIClassifier-Released: {stamp} by dashboard (false positive, re-injected)\r\n");
    let mut payload = note.into_bytes();
    payload.extend_from_slice(eml.as_bytes());

    let mut cmd = std::process::Command::new(sendmail_cmd());
    cmd.arg("-i") // dot-stuffing off: we deliver a complete message
        .arg("-oi")
        .arg("-f")
        .arg(&from)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(ReleaseError::Delivery(format!(
                "cannot run {}: {e}",
                sendmail_cmd()
            )))
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if let Err(e) = stdin.write_all(&payload) {
            let _ = child.wait();
            return Err(ReleaseError::Delivery(format!(
                "write to sendmail failed: {e}"
            )));
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return Err(ReleaseError::Delivery(format!("sendmail wait failed: {e}"))),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(ReleaseError::Delivery(format!(
            "sendmail exited {}: {}",
            out.status,
            err.trim().chars().take(200).collect::<String>()
        )));
    }

    // Audit trail next to the eval log: who released what, from where.
    if let Some(log) = eval_log {
        let line = serde_json::json!({
            "ts": stamp.to_string(),
            "event": "released",
            "stem": stem,
            "envelope_from": from,
            "score": meta_v.get("outcomes")
                .and_then(|o| o.as_array())
                .and_then(|a| {
                    a.iter()
                        .filter_map(|x| x.get("score").and_then(|s| s.as_f64()))
                        .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v))))
                }),
        });
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .and_then(|mut f| std::io::Write::write_all(&mut f, format!("{line}\n").as_bytes()));
    }
    Ok(from)
}

// ------------------------------------------------------------ quarantine cache
//
// Parsing every sidecar JSON on every request is wasteful once the quarantine
// holds more than a handful of mails. The cache lives next to the data (inside
// `spam_dir`) so it survives a restart and can simply be deleted to force a
// rebuild.
//
// Versioning: `CACHE_VERSION` is part of the stored payload. Bump it whenever
// the sidecar format or the parsing changes — an older cache is then discarded
// and rebuilt from the current files instead of serving stale data.

const CACHE_VERSION: u32 = 2;
const CACHE_FILE: &str = ".dashboard-cache.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    /// Stamp of the **sidecar JSON**, because that is what we parsed. Keying
    /// on the `.eml` instead would miss a sidecar-only rewrite and serve stale
    /// scores. Nanoseconds to avoid same-second collisions.
    pub sidecar_mtime_nanos: u128,
    pub sidecar_size: u64,
    pub mail: QuarantineMail,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CacheFile {
    version: u32,
    entries: std::collections::HashMap<String, CacheEntry>,
}

/// In-memory cache of parsed sidecars, keyed by mail stem.
#[derive(Debug, Default)]
pub struct QuarantineCache {
    entries: std::collections::HashMap<String, CacheEntry>,
    dirty: bool,
    /// Path of the on-disk cache file (inside spam_dir).
    path: Option<PathBuf>,
}

impl QuarantineCache {
    /// Load from disk. A version mismatch or corrupt file starts an empty
    /// cache instead of failing — the quarantine is the source of truth.
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(CACHE_FILE);
        let mut c = QuarantineCache {
            entries: Default::default(),
            dirty: false,
            path: Some(path.clone()),
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return c;
        };
        match serde_json::from_str::<CacheFile>(&raw) {
            Ok(f) if f.version == CACHE_VERSION => {
                tracing::info!(entries = f.entries.len(), "dashboard cache loaded");
                c.entries = f.entries;
            }
            Ok(f) => {
                tracing::info!(
                    found = f.version,
                    expected = CACHE_VERSION,
                    "dashboard cache version changed, discarding"
                );
                // Don't delete the file here: it may belong to a rollback.
                // Just ignore it and overwrite on the next save.
            }
            Err(e) => {
                tracing::warn!(error = %e, "dashboard cache unreadable, rebuilding");
            }
        }
        c
    }

    /// Cached parse, or `None` when the sidecar changed since it was cached.
    fn get(&self, stem: &str, stamp: (u128, u64)) -> Option<&QuarantineMail> {
        let e = self.entries.get(stem)?;
        if e.sidecar_mtime_nanos == stamp.0 && e.sidecar_size == stamp.1 {
            Some(&e.mail)
        } else {
            None
        }
    }

    fn put(&mut self, stamp: (u128, u64), mail: QuarantineMail) {
        let stem = mail.stem.clone();
        self.entries.insert(
            stem,
            CacheEntry {
                sidecar_mtime_nanos: stamp.0,
                sidecar_size: stamp.1,
                mail,
            },
        );
        self.dirty = true;
    }

    /// Drop entries whose `.eml` no longer exists (retention/rotation).
    fn retain_existing(&mut self, live: &std::collections::HashSet<String>) {
        let before = self.entries.len();
        self.entries.retain(|k, _| live.contains(k));
        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    /// Persist if changed. Failure is logged, never fatal: the cache is an
    /// optimisation, not a source of truth.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        let file = CacheFile {
            version: CACHE_VERSION,
            entries: std::mem::take(&mut self.entries),
        };
        // Build the payload before touching the file so a serialization
        // failure cannot wipe the in-memory cache.
        match serde_json::to_vec(&file) {
            Ok(bytes) => {
                if let Err(e) = atomic_write(&path, &bytes) {
                    tracing::warn!(error = %e, "dashboard cache write failed");
                    self.entries = file.entries; // restore
                    return;
                }
                self.entries = file.entries;
                self.dirty = false;
            }
            Err(e) => {
                tracing::warn!(error = %e, "dashboard cache serialize failed");
                self.entries = file.entries; // restore
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Write via a temp file + rename so a crash mid-write cannot leave a
/// half-written cache behind.
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// (mtime in nanos since epoch, size) of the sidecar JSON. (0, 0) when missing,
/// which is a stable key so a missing sidecar stays cached as "no data".
fn sidecar_stamp(dir: &Path, stem: &str) -> (u128, u64) {
    std::fs::metadata(dir.join(format!("{stem}.json")))
        .map(|m| {
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            (t, m.len())
        })
        .unwrap_or((0, 0))
}

/// List quarantined mails, newest first. `q` filters stem/sender substring (case-insensitive).
pub fn list_quarantine(
    dir: &Path,
    q: &str,
    limit: usize,
    offset: usize,
    cache: &mut QuarantineCache,
) -> (Vec<QuarantineMail>, usize) {
    let mut all: Vec<QuarantineMail> = Vec::new();
    let mut live = std::collections::HashSet::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return (Vec::new(), 0),
    };
    for e in rd.flatten() {
        let path = e.path();
        if path
            .extension()
            .map(|x| !x.eq_ignore_ascii_case("eml"))
            .unwrap_or(true)
        {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem.is_empty() {
            continue;
        }
        let (mtime_unix, size) = e
            .metadata()
            .map(|m| {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                (t, m.len())
            })
            .unwrap_or((0, 0));
        live.insert(stem.clone());

        // Fast path: unchanged sidecar -> reuse the parsed result.
        let stamp = sidecar_stamp(dir, &stem);
        let mail = match cache.get(&stem, stamp) {
            Some(m) => m.clone(),
            None => {
                let (sender, spam, verdict, summary, models) = read_sidecar_json(dir, &stem);
                let m = QuarantineMail {
                    stem: stem.clone(),
                    mtime_unix,
                    size,
                    sender,
                    spam,
                    verdict,
                    summary,
                    models,
                };
                cache.put(stamp, m.clone());
                m
            }
        };
        all.push(mail);
    }
    cache.retain_existing(&live);
    all.sort_by(|a, b| b.mtime_unix.cmp(&a.mtime_unix).then(a.stem.cmp(&b.stem)));
    if !q.is_empty() {
        let ql = q.to_lowercase();
        all.retain(|m| {
            m.stem.to_lowercase().contains(&ql) || m.sender.to_lowercase().contains(&ql)
        });
    }
    let total = all.len();
    let items = all.into_iter().skip(offset).take(limit.min(500)).collect();
    (items, total)
}

fn read_sidecar_json(
    dir: &Path,
    stem: &str,
) -> (String, Option<bool>, Option<String>, String, Vec<ModelChip>) {
    let empty = || (String::new(), None, None, String::new(), Vec::new());
    let Ok(s) = std::fs::read_to_string(dir.join(format!("{stem}.json"))) else {
        return empty();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else {
        return empty();
    };
    let sender = v
        .get("envelope_from")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let verdict = v
        .get("verdict")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let outcomes = v
        .get("outcomes")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let models: Vec<ModelChip> = outcomes
        .iter()
        .take(8)
        .map(|o| {
            let model = o.get("model").and_then(|x| x.as_str()).unwrap_or("?");
            ModelChip {
                name: short_model(model),
                spam: o.get("is_spam").and_then(|x| x.as_bool()).unwrap_or(false),
                err: o.get("error").and_then(|x| x.as_str()).is_some(),
                // ModelOutcome serializes LlmVerdict with #[serde(flatten)],
                // so `score` sits at the top level. Accept a nested object too
                // so hand-written or older sidecars still parse.
                score: o
                    .get("score")
                    .or_else(|| o.get("verdict").and_then(|v| v.get("score")))
                    .and_then(|x| x.as_f64())
                    .unwrap_or(0.0) as f32,
            }
        })
        .collect();
    // Compact summary like the X-AIClassifier-Verdict tag (fallback for JSON readers).
    let mut parts: Vec<String> = Vec::new();
    for m in models.iter().take(6) {
        if m.err {
            parts.push(format!("{}=err", m.name));
        } else {
            parts.push(format!(
                "{}={}:{:.2}",
                m.name,
                if m.spam { "spam" } else { "ham" },
                m.score
            ));
        }
    }
    let summary: String = parts.join(" | ").chars().take(160).collect();
    // spam flag is not stored in the sidecar; derive from outcomes when present.
    let spam = if outcomes.is_empty() {
        None
    } else {
        Some(models.iter().any(|m| m.spam))
    };
    (sender, spam, verdict, summary, models)
}

/// Cap for the in-page source preview so one huge mail cannot blow up the page.
pub const PREVIEW_LIMIT: usize = 200_000;

/// Read meta + the *full* message. Used by `/raw`, which must never truncate:
/// the whole point of that route is getting the untouched file.
pub fn read_mail_raw(dir: &Path, stem: &str) -> Option<(String, String)> {
    let eml = read_eml(dir, stem)?;
    let meta =
        std::fs::read_to_string(dir.join(format!("{stem}.json"))).unwrap_or_else(|_| "{}".into());
    Some((meta, eml))
}

/// Read meta + a preview capped at `PREVIEW_LIMIT`, for the HTML view.
pub fn read_mail(dir: &Path, stem: &str) -> Option<(String, String)> {
    let eml = read_eml(dir, stem)?;
    let meta =
        std::fs::read_to_string(dir.join(format!("{stem}.json"))).unwrap_or_else(|_| "{}".into());
    let text =
        String::from_utf8_lossy(&eml.as_bytes()[..eml.len().min(PREVIEW_LIMIT)]).into_owned();
    Some((meta, text))
}

/// Shared traversal guard + read. Returns the lossless message as a String.
fn read_eml(dir: &Path, stem: &str) -> Option<String> {
    if stem.contains('/') || stem.contains('\\') || stem.contains("..") {
        return None;
    }
    std::fs::read(dir.join(format!("{stem}.eml")))
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

// ------------------------------------------------------------------ http utils

pub fn html_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            _ => o.push(c),
        }
    }
    o
}

/// Percent-decode to bytes first so multi-byte UTF-8 (%C3%A4 -> ä) survives.
/// `plus` (space for `+`) applies to query strings only, not to paths.
fn decode_component(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(if plus && b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Minimal percent-encoding for generated path segments (stems).
fn url_encode(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                o.push(b as char)
            }
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

pub fn parse_query(qs: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((k, v)) => {
                m.insert(decode_component(k, true), decode_component(v, true));
            }
            None => {
                m.insert(decode_component(pair, true), String::new());
            }
        }
    }
    m
}

fn parse_usize(s: Option<&String>, dflt: usize) -> usize {
    s.and_then(|v| v.parse().ok()).unwrap_or(dflt)
}

const CSS: &str = ":root{color-scheme:light}\
*{box-sizing:border-box}\
body{font-family:system-ui,-apple-system,'Segoe UI',Roboto,sans-serif;max-width:1080px;margin:0 auto;padding:0 1.2rem 3rem;color:#1c2430;background:#f6f8fb}\
nav.top{display:flex;gap:1.3rem;align-items:center;background:#16233a;color:#fff;margin:0 -1.2rem 1.4rem;padding:.65rem 1.2rem;position:sticky;top:0;z-index:10}\
nav.top a{color:#cfe0ff;text-decoration:none;font-weight:600}\
nav.top a:hover{color:#fff;text-decoration:underline}\
nav.top .sp{flex:1}\
h1{font-size:1.5rem;margin:.4rem 0 1rem}\
h2{font-size:1.1rem;margin:1.6rem 0 .4rem;color:#33415c}\
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(120px,1fr));gap:.7rem;margin:1rem 0}\
.card{background:#fff;border:1px solid #e2e8f0;border-radius:12px;padding:.7rem .4rem;text-align:center;box-shadow:0 1px 2px rgba(20,40,80,.06)}\
.card b{font-size:1.5rem;display:block;font-variant-numeric:tabular-nums}\
.card span{font-size:.75rem;color:#64748b}\
.card.hot b{color:#c2410c}\
table{border-collapse:collapse;width:100%;margin:.6rem 0;font-size:.84rem;background:#fff;border-radius:12px;overflow:hidden;box-shadow:0 1px 2px rgba(20,40,80,.06)}\
th,td{padding:.45rem .6rem;text-align:left;vertical-align:top;border-bottom:1px solid #eef2f7}\
th{background:#eef3f9;color:#33415c;font-size:.76rem;text-transform:uppercase;letter-spacing:.03em}\
tr:last-child td{border-bottom:none}\
.badge{display:inline-block;padding:.1rem .55rem;border-radius:999px;font-size:.75rem;font-weight:700}\
.badge.s{background:#fee2e2;color:#b91c1c}\
.badge.h{background:#dcfce7;color:#15803d}\
.badge.w{background:#e2e8f0;color:#475569}\
.badge.t{background:#fef3c7;color:#b45309}\
.status{display:flex;align-items:center;gap:.5rem;font-size:.82rem;color:#64748b;margin:.2rem 0 .6rem}\
.dot{font-size:.9rem}\
.dot.on{color:#16a34a}\
.dot.off{color:#dc2626}\
a{color:#1d4ed8}\
pre{background:#0f1a2e;color:#dbe7ff;padding:1rem;border-radius:12px;overflow:auto;white-space:pre-wrap;font-size:.8rem}\
.src{position:relative}\
.src .body{white-space:pre;overflow-x:auto;max-height:72vh;user-select:all;\
-webkit-user-select:all;cursor:text}\
.src .body.wrap{white-space:pre-wrap;overflow-x:hidden}\
.src .bar{display:flex;gap:.6rem;align-items:center;justify-content:space-between;\
background:#e8eef7;color:#33415c;font-size:.76rem;padding:.35rem .6rem;border-radius:10px 10px 0 0}\
.src .bar button{padding:.2rem .6rem;font-size:.72rem}\
p.warn{background:#fef3c7;border:1px solid #fde68a;color:#92400e;\
padding:.5rem .7rem;border-radius:8px;font-size:.82rem}\
::selection{background:#93c5fd;color:#0b1220}\
form{display:flex;gap:.5rem;margin:1rem 0;flex-wrap:wrap}\
input[type=text]{padding:.45rem .6rem;width:300px;border:1px solid #cbd5e1;border-radius:8px}\
button{padding:.45rem 1rem;border:none;border-radius:8px;background:#1d4ed8;color:#fff;font-weight:600;cursor:pointer}\
button:hover{background:#1e40af}\
.pager{margin:1rem 0}\
.scr{filter:blur(4px);user-select:none;pointer-events:none}\
.chip{display:inline-block;margin:.12rem .18rem .12rem 0;padding:.12rem .55rem;border-radius:7px;\
font-size:.76rem;font-weight:600;background:#f1f5f9;border:1px solid #e2e8f0;color:#33415c;\
font-variant-numeric:tabular-nums;white-space:nowrap}\
.chip.s{background:#fee2e2;border-color:#fecaca;color:#991b1c}\
.chip.h{background:#dcfce7;border-color:#bbf7d0;color:#166534}\
.chip.e{background:#e2e8f0;border-color:#cbd5e1;color:#475569}";

/// Live refresh for the overview page: polls /api/stats every 3s and updates
/// cards + tables in place (plain HTTP fetch, no WebSocket/SSE needed).
const LIVE_JS: &str = r#"<script>
const tok = new URLSearchParams(location.search).get('token') || '';
const api = tok ? '/api/stats?token=' + encodeURIComponent(tok) : '/api/stats';
const esc = s => String(s == null ? '' : s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const badge = (t, c) => '<span class="badge ' + c + '">' + esc(t) + '</span>';
const chip = m => {
  const cls = m.err ? 'e' : (m.spam ? 's' : 'h');
  const v = m.err ? 'err' : ((m.spam ? 'SPAM ' : 'ham ') + Number(m.score || 0).toFixed(2));
  return '<span class="chip ' + cls + '">' + esc(m.name + ' · ' + v) + '</span>';
};
const set = (id, v) => { const el = document.getElementById(id); if (el) el.textContent = v; };
async function tick() {
  const dot = document.getElementById('m-dot');
  try {
    const r = await fetch(api, {cache: 'no-store'});
    if (!r.ok) throw new Error(r.status);
    const d = await r.json();
    set('c-total', d.total); set('c-spam', d.spam); set('c-ham', d.ham);
    set('c-wl', d.whitelisted); set('c-terr', d.temperror); set('c-q', d.quarantine_files);
    const models = Object.entries(d.per_model || {}).sort((a, b) => a[0] < b[0] ? -1 : 1);
    document.getElementById('m-models').innerHTML = models.map(([n, m]) =>
      '<tr><td>' + esc(n) + '</td><td>' + m.ok + '</td><td>' + m.spam + '</td><td>' + m.errors +
      '</td><td>' + Number(m.avg_score || 0).toFixed(3) + '</td><td>' + m.avg_latency_ms +
      'ms</td><td>$' + Number(m.cost_usd || 0).toFixed(5) + '</td></tr>').join('')
      || '<tr><td colspan="7"><i>no data</i></td></tr>';
    document.getElementById('m-recent').innerHTML = (d.recent || []).slice(0, 20).map(x => {
      const v = x.whitelisted ? badge('whitelisted', 'w') : x.temperror ? badge('temperror', 't')
        : x.spam ? badge('SPAM', 's') : badge('ham', 'h');
      const cls = (typeof SCRAMBLE !== 'undefined' && SCRAMBLE) ? ' class="scr"' : '';
      const det = (x.models && x.models.length) ? x.models.map(chip).join(' ') : esc(x.summary);
      return '<tr><td>' + esc(x.ts) + '</td><td>' + esc(x.queue_id) + '</td><td' + cls + '>' + esc(x.sender) +
        '</td><td>' + v + '</td><td>' + det + '</td></tr>';
    }).join('') || '<tr><td colspan="5"><i>No mails since start.</i></td></tr>';
    set('m-updated', 'live · updated ' + new Date().toLocaleTimeString());
    if (dot) { dot.textContent = '●'; dot.className = 'dot on'; }
  } catch (e) {
    if (dot) { dot.textContent = '●'; dot.className = 'dot off'; }
    set('m-updated', 'offline — retrying …');
  }
}
tick(); setInterval(tick, 3000);
</script>"#;

/// Internal link builder, preserving ?token= and ?scramble= when present.
/// Explicit `params` override kept keys (used for the scramble toggle).
fn href_q(base: &str, query: &HashMap<String, String>, params: &[(&str, &str)]) -> String {
    let over: Vec<&str> = params.iter().map(|(k, _)| *k).collect();
    let mut q: Vec<String> = Vec::new();
    for key in ["token", "scramble"] {
        if over.contains(&key) {
            continue;
        }
        if let Some(v) = query.get(key) {
            q.push(format!("{key}={}", url_encode(v)));
        }
    }
    for (k, v) in params {
        q.push(format!("{k}={}", url_encode(v)));
    }
    if q.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", q.join("&"))
    }
}

/// Hidden form fields carrying token/scramble through GET forms.
fn hidden_keep(query: &HashMap<String, String>) -> String {
    let mut h = String::new();
    for key in ["token", "scramble"] {
        if let Some(v) = query.get(key) {
            h.push_str(&format!(
                "<input type=hidden name={key} value=\"{}\">",
                html_escape(v)
            ));
        }
    }
    h
}

/// Effective sender scrambling: ?scramble=1/0 wins, else the config default.
fn eff_scramble(query: &HashMap<String, String>, default: bool) -> bool {
    match query.get("scramble").map(String::as_str) {
        Some("1") | Some("true") | Some("on") => true,
        Some("0") | Some("false") | Some("off") => false,
        _ => default,
    }
}

/// Display-only sender redaction for <pre> blocks (CSS can't target substrings).
fn redact_sender(text: &str, sender: &str) -> String {
    if sender.is_empty() {
        return text.to_string();
    }
    text.replace(sender, "•••@•••")
}

/// Badge for the quarantine list: prefer the stored verdict
/// ("spam"|"ham"|"whitelisted"), fall back to the derived spam flag.
fn mail_badge(m: &QuarantineMail) -> &'static str {
    match m.verdict.as_deref() {
        Some("whitelisted") => "<span class=\"badge w\">whitelisted</span>",
        Some("spam") => "<span class=\"badge s\">SPAM</span>",
        Some("ham") => "<span class=\"badge h\">ham</span>",
        _ => match m.spam {
            Some(true) => "<span class=\"badge s\">SPAM</span>",
            Some(false) => "<span class=\"badge h\">ham</span>",
            None => "-",
        },
    }
}

fn verdict_badge(whitelisted: bool, temperror: bool, spam: bool) -> &'static str {
    if whitelisted {
        "<span class=\"badge w\">whitelisted</span>"
    } else if temperror {
        "<span class=\"badge t\">temperror</span>"
    } else if spam {
        "<span class=\"badge s\">SPAM</span>"
    } else {
        "<span class=\"badge h\">ham</span>"
    }
}

fn page(
    title: &str,
    body: &str,
    query: &HashMap<String, String>,
    script: &str,
    scr: bool,
    here: &str,
) -> String {
    let toggle = if scr { "0" } else { "1" };
    let nav = format!(
        "<nav class=top><a href=\"{}\">Stats</a><a href=\"{}\">Quarantine</a><span class=sp></span>\
        <a href=\"{}\">Scramble: {}</a><a href=\"{}\">JSON</a></nav>",
        href_q("/", query, &[]),
        href_q("/mails", query, &[]),
        href_q(here, query, &[("scramble", toggle)]),
        if scr { "on" } else { "off" },
        href_q("/api/stats", query, &[]),
    );
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
        <title>{}</title><style>{}</style></head><body>{}<h1>{}</h1>{}{}</body></html>",
        html_escape(title),
        CSS,
        nav,
        html_escape(title),
        body,
        script
    )
}

// ------------------------------------------------------------------ server

pub struct DashboardOpts {
    pub stats: Arc<Stats>,
    pub spam_dir: PathBuf,
    pub token: Option<String>,
    pub scramble_senders: bool, // config default; ?scramble=1/0 overrides per request
    /// Parsed-sidecar cache. Mutated on read, so it is shared behind a Mutex
    /// across the request handlers spawned by `serve`.
    pub cache: Arc<std::sync::Mutex<QuarantineCache>>,
    /// Where release audit entries are appended (eval.jsonl). None disables it.
    pub eval_log: Option<PathBuf>,
    /// Master switch for the "deliver anyway" button.
    pub release_enabled: bool,
}

fn authorized(headers: &str, query: &HashMap<String, String>, token: &Option<String>) -> bool {
    let Some(t) = token.as_ref() else { return true };
    if query.get("token").map(|v| v == t).unwrap_or(false) {
        return true;
    }
    for line in headers.lines().skip(1) {
        let line = line.trim_end_matches('\r');
        // Byte-level compare: header names are ASCII, avoids panics on weird input.
        let b = line.as_bytes();
        if b.len() > 13 && b[13] == b':' && b[..13].eq_ignore_ascii_case(b"authorization") {
            let v = line[b.len().min(14)..].trim();
            if v == format!("Bearer {t}") {
                return true;
            }
        }
        if line.is_empty() {
            break;
        }
    }
    false
}

fn resp(status: &str, ctype: &str, body: &[u8]) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    [head.as_bytes(), body].concat()
}

fn handle_request(req: &str, opts: &DashboardOpts) -> Vec<u8> {
    let method = req.lines().next().unwrap_or("").split_whitespace().next();
    if method == Some("POST") {
        return handle_post(req, opts);
    }
    let head_only = method == Some("HEAD");
    let mut out = handle_get(req, opts);
    if head_only {
        if let Some(hdr_end) = out.windows(4).position(|w| w == b"\r\n\r\n") {
            out.truncate(hdr_end + 4);
        }
    }
    out
}

/// POST is only used for the release action — a state change that must never
/// be triggerable by a link, a prefetch or a screenshot click.
fn handle_post(req: &str, opts: &DashboardOpts) -> Vec<u8> {
    let request_line = req.lines().next().unwrap_or("").trim_end_matches('\r');
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, qs) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let query = parse_query(qs);
    if !authorized(req, &query, &opts.token) {
        return resp("401 Unauthorized", "text/plain", b"missing or wrong token");
    }
    let path = decode_component(path, false);
    let Some(stem) = path
        .strip_prefix("/mails/")
        .and_then(|s| s.strip_suffix("/release"))
    else {
        return resp("404 Not Found", "text/plain", b"not found");
    };
    if stem.is_empty() || stem.contains('/') {
        return resp("404 Not Found", "text/plain", b"not found");
    }
    // Feature flag last: a wrong path is a 404 even when release is disabled.
    if !opts.release_enabled {
        return resp("403 Forbidden", "text/plain", b"release is disabled");
    }
    match release_mail(&opts.spam_dir, stem, opts.eval_log.as_deref()) {
        Ok(from) => {
            tracing::info!(stem = %stem, from = %from, "quarantined mail released");
            let body = format!(
                "<p><b>Released.</b> {} was handed to the MTA via sendmail (envelope from {}).</p>\
                 <p>It will not pass the milter again (pickup bypasses milters).</p>\
                 <p><a href=\"{}\">&larr; back</a></p>",
                html_escape(stem),
                html_escape(&from),
                href_q("/mails", &query, &[])
            );
            resp(
                "200 OK",
                "text/html; charset=utf-8",
                page("Released", &body, &query, "", false, "/mails").as_bytes(),
            )
        }
        Err(ReleaseError::Invalid(m)) => {
            tracing::warn!(stem = %stem, error = %m, "release rejected");
            resp("400 Bad Request", "text/plain", m.as_bytes())
        }
        Err(ReleaseError::Delivery(m)) => {
            tracing::warn!(stem = %stem, error = %m, "release delivery failed");
            resp("502 Bad Gateway", "text/plain", m.as_bytes())
        }
    }
}

fn handle_get(req: &str, opts: &DashboardOpts) -> Vec<u8> {
    let mut lines = req.lines();
    let request_line = lines
        .next()
        .unwrap_or("")
        .trim_end_matches('\r')
        .to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    if method != "GET" && method != "HEAD" {
        return resp(
            "405 Method Not Allowed",
            "text/plain",
            b"only GET supported",
        );
    }
    let (path, qs) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let query = parse_query(qs);
    if !authorized(req, &query, &opts.token) {
        return resp("401 Unauthorized", "text/plain", b"missing or wrong token");
    }
    let path = decode_component(path, false);
    tracing::debug!(method = method, path = %path, "dashboard request");
    let scr = eff_scramble(&query, opts.scramble_senders);

    if path == "/" {
        let qc = quarantine_count(&opts.spam_dir);
        let mut body = opts.stats.overview_cards(qc);
        body.push_str(&opts.stats.per_model_html());
        body.push_str("<h2>Recent mails</h2>");
        body.push_str(&opts.stats.recent_html(scr));
        body.push_str(&format!(
            "<p><a href=\"{}\">Browse spam quarantine &rarr;</a></p>",
            href_q("/mails", &query, &[])
        ));
        let script = format!(
            "<script>const SCRAMBLE={};</script>{LIVE_JS}",
            if scr { "true" } else { "false" }
        );
        return resp(
            "200 OK",
            "text/html; charset=utf-8",
            page("Milter-Stats", &body, &query, &script, scr, "/").as_bytes(),
        );
    }
    if path == "/api/stats" {
        let qc = quarantine_count(&opts.spam_dir);
        let mut snap = opts.stats.snapshot_json(qc);
        if scr {
            if let Some(arr) = snap.get_mut("recent").and_then(|r| r.as_array_mut()) {
                for r in arr.iter_mut() {
                    r["sender"] = serde_json::Value::String("•••@•••".into());
                }
            }
        }
        let body = serde_json::to_string(&snap).unwrap_or_default();
        return resp("200 OK", "application/json", body.as_bytes());
    }
    if path == "/mails" || path == "/mails/" || path == "/api/mails" {
        let q = query.get("q").cloned().unwrap_or_default();
        let limit = parse_usize(query.get("limit"), 100).min(500);
        let offset = parse_usize(query.get("offset"), 0);
        // Parse-heavy: lock only for the cache, never across I/O.
        let (items, total) = {
            let mut guard = match opts.cache.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let r = list_quarantine(&opts.spam_dir, &q, limit, offset, &mut guard);
            guard.save();
            r
        };
        if path.starts_with("/api") {
            let body = serde_json::to_string(&serde_json::json!({
                "total": total, "limit": limit, "offset": offset,
                "items": items.iter().map(|m| serde_json::json!({
                    "stem": m.stem, "mtime_unix": m.mtime_unix, "size": m.size,
                    "sender": if scr { "•••@•••" } else { &m.sender },
                    "spam": m.spam, "verdict": m.verdict, "summary": m.summary, "models": m.models,
                })).collect::<Vec<_>>(),
            }))
            .unwrap_or_default();
            return resp("200 OK", "application/json", body.as_bytes());
        }
        let scr_cls = if scr { " class=\"scr\"" } else { "" };
        let mut body = format!(
            "<form method=get action=\"{}\"><input type=text name=q placeholder=\"Search (sender/filename)\" value=\"{}\"{scr_cls}>{}\
            <button>Search</button></form><p>{} matches (showing {} from {})</p>",
            href_q("/mails", &HashMap::new(), &[]),
            html_escape(&q),
            hidden_keep(&query),
            total,
            items.len(),
            offset
        );
        body.push_str("<table><tr><th>Date</th><th>File</th><th>Sender</th><th>Verdict</th><th>Models</th><th>Action</th></tr>");
        for m in &items {
            let dt = chrono::DateTime::from_timestamp(m.mtime_unix as i64, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "-".into());
            let detail = if m.models.is_empty() {
                html_escape(&m.summary)
            } else {
                chips_html(&m.models)
            };
            // Release button: POST only, confirmed in the browser, so a
            // prefetch or a stray click cannot deliver a quarantined mail.
            let release_btn = if opts.release_enabled && m.spam == Some(true) {
                format!(
                    "<form method=post action=\"{}\" onsubmit=\"return confirm('Release {}? It will be delivered.')\">\
                     <button class=rel>Deliver anyway</button></form>",
                    href_q(
                        &format!("/mails/{}/release", url_encode(&m.stem)),
                        &query,
                        &[]
                    ),
                    html_escape(&m.stem)
                )
            } else {
                String::new()
            };
            body.push_str(&format!(
                "<tr><td>{dt}</td><td><a href=\"{}\">{}</a> ({} KB)</td><td{scr_cls}>{}</td><td>{}</td><td>{}</td><td>{release_btn}</td></tr>",
                href_q(&format!("/mails/{}", url_encode(&m.stem)), &query, &[]),
                html_escape(&m.stem),
                m.size / 1024,
                html_escape(&m.sender),
                mail_badge(m),
                detail
            ));
        }
        body.push_str("</table>");
        if total > offset + items.len() {
            body.push_str(&format!(
                "<p class=pager><a href=\"{}\">next &rarr;</a></p>",
                href_q(
                    "/mails",
                    &query,
                    &[
                        ("q", &q),
                        ("limit", &limit.to_string()),
                        ("offset", &(offset + items.len()).to_string())
                    ]
                ),
            ));
        }
        return resp(
            "200 OK",
            "text/html; charset=utf-8",
            page("Spam quarantine", &body, &query, "", scr, "/mails").as_bytes(),
        );
    }
    if let Some(stem) = path.strip_prefix("/mails/") {
        let (stem, raw) = match stem.strip_suffix("/raw") {
            Some(s) => (s.to_string(), true),
            None => (stem.to_string(), false),
        };
        if stem.is_empty() || stem.contains('/') {
            return resp("404 Not Found", "text/plain", b"not found");
        }
        // /raw must serve the complete file, the HTML view may cap the preview.
        match if raw {
            read_mail_raw(&opts.spam_dir, &stem)
        } else {
            read_mail(&opts.spam_dir, &stem)
        } {
            Some((_meta, eml)) if raw => {
                return resp("200 OK", "text/plain; charset=utf-8", eml.as_bytes());
            }
            Some((meta, eml)) => {
                let sender = serde_json::from_str::<serde_json::Value>(&meta)
                    .ok()
                    .and_then(|v| {
                        v.get("envelope_from")
                            .and_then(|x| x.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                let pretty = serde_json::from_str::<serde_json::Value>(&meta)
                    .map(|v| serde_json::to_string_pretty(&v).unwrap_or(meta.clone()))
                    .unwrap_or(meta);
                let (pretty, eml) = if scr {
                    (
                        redact_sender(&pretty, &sender),
                        redact_sender(&eml, &sender),
                    )
                } else {
                    (pretty, eml)
                };
                let here = format!("/mails/{}", url_encode(&stem));
                // Container for the raw source: `user-select:all` makes a
                // double-click select the whole message, and `white-space:pre`
                // (not pre-wrap) keeps the copied text byte-identical — soft
                // wraps would otherwise be pasted in as line breaks.
                let truncated = if eml.len() >= PREVIEW_LIMIT {
                    format!(
                        "<p class=warn>Preview truncated at {} KB &mdash; use <a href=\"{}\">raw .eml</a> for the complete message.</p>",
                        PREVIEW_LIMIT / 1000,
                        href_q(&format!("{here}/raw"), &query, &[])
                    )
                } else {
                    String::new()
                };
                let body = format!(
                    "<p><a href=\"{}\">&larr; back</a> &middot; <a href=\"{}\">raw .eml</a></p>\
                    <h2>Meta</h2><pre>{}</pre>\
                    <h2>Mail source</h2>\
                    <div class=src><div class=bar>\
                    <span>double-click to select &middot; copy button fetches the byte-exact source</span>\
                    <span>\
                    <button type=button id=cpbtn>copy exact</button> \
                    <button type=button id=wrapbtn>wrap</button></span>\
                    </div><pre class=body id=mailsrc>{}</pre></div>{truncated}\
                    <script>\
                    (function(){{\
                    var b=document.getElementById('mailsrc');\
                    document.getElementById('wrapbtn').onclick=function(){{\
                    b.classList.toggle('wrap');\
                    this.textContent=b.classList.contains('wrap')?'no wrap':'wrap';}};\
                    document.getElementById('cpbtn').onclick=async function(){{\
                    var btn=this;\
                    try {{\
                    var r=await fetch('{}',{{cache:'no-store'}});\
                    var txt=await r.text();\
                    if(navigator.clipboard&&navigator.clipboard.writeText){{\
                    await navigator.clipboard.writeText(txt);\
                    }} else {{\
                    var ta=document.createElement('textarea');ta.value=txt;\
                    document.body.appendChild(ta);ta.select();\
                    document.execCommand('copy');document.body.removeChild(ta);\
                    }}\
                    btn.textContent='copied';\
                    }} catch(e) {{ btn.textContent='copy failed'; }}\
                    setTimeout(function(){{btn.textContent='copy exact'}},1500);\
                    }};}})();\
                    </script>",
                    href_q("/mails", &query, &[]),
                    href_q(&format!("{here}/raw"), &query, &[]),
                    html_escape(&pretty),
                    html_escape(&eml),
                    // Copy pulls from /raw: the rendered HTML normalises CRLF to
                    // LF, which would corrupt a pasted mail.
                    href_q(&format!("{here}/raw"), &query, &[])
                );
                return resp(
                    "200 OK",
                    "text/html; charset=utf-8",
                    page(&stem, &body, &query, "", scr, &here).as_bytes(),
                );
            }
            None => return resp("404 Not Found", "text/plain", b"mail not found"),
        }
    }
    if let Some(stem) = path.strip_prefix("/api/mails/") {
        if stem.is_empty() || stem.contains('/') {
            return resp(
                "404 Not Found",
                "application/json",
                br#"{"error":"not found"}"#,
            );
        }
        // The API is for tooling: serve the complete message.
        match read_mail_raw(&opts.spam_dir, stem) {
            Some((meta, eml)) => {
                let meta_v: serde_json::Value =
                    serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null);
                let body = serde_json::to_string(
                    &serde_json::json!({"stem": stem, "meta": meta_v, "eml": eml}),
                )
                .unwrap_or_default();
                return resp("200 OK", "application/json", body.as_bytes());
            }
            None => {
                return resp(
                    "404 Not Found",
                    "application/json",
                    br#"{"error":"not found"}"#,
                )
            }
        }
    }
    resp("404 Not Found", "text/plain", b"not found")
}

pub async fn serve(listen: String, opts: DashboardOpts) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(&listen).await?;
    tracing::info!(listen = %listen, "dashboard listening");
    let opts = Arc::new(opts);
    loop {
        let (mut stream, _) = listener.accept().await?;
        let opts = opts.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;
            // Read until end of headers (or cap): a single read() may return
            // a segmented request. GET has no body, so headers are enough.
            let mut buf = vec![0u8; 0];
            buf.reserve(4096);
            let mut tmp = [0u8; 4096];
            let complete = loop {
                match stream.read(&mut tmp).await {
                    Ok(0) => break !buf.is_empty(),
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.len() > 65_536 {
                            break false; // oversize junk, drop
                        }
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break true;
                        }
                    }
                    Err(_) => break false,
                }
            };
            if !complete {
                return;
            }
            let req = String::from_utf8_lossy(&buf).into_owned();
            let out = handle_request(&req, &opts);
            let _ = stream.write_all(&out).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal temp dir helper (no `tempfile` dependency): returns a unique
    /// path that is removed on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "aim-dash-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).expect("create temp dir");
            TempDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, name: &str, body: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, body).expect("write temp file");
            p
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn opts_for(dir: &Path, token: Option<&str>, scramble: bool) -> DashboardOpts {
        DashboardOpts {
            stats: Stats::new(),
            spam_dir: dir.to_path_buf(),
            token: token.map(str::to_string),
            scramble_senders: scramble,
            cache: Arc::new(std::sync::Mutex::new(QuarantineCache::load(dir))),
            eval_log: None,
            release_enabled: false,
        }
    }

    /// GET a route and split the response into (status_line, body).
    fn get(opts: &DashboardOpts, target: &str) -> (String, String) {
        let out = handle_request(&format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n"), opts);
        let text = String::from_utf8_lossy(&out).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        let status = head.lines().next().unwrap_or("").to_string();
        (status, body.to_string())
    }

    fn quarantine_with_two_mails() -> TempDir {
        let td = TempDir::new("quarantine");
        td.write(
            "20240101T000000_q1_spam.eml",
            "From: spammer@evil.com\r\nSubject: win\r\n\r\nbody",
        );
        // Shape mirrors the real ModelOutcome serde output: LlmVerdict is
        // #[serde(flatten)]ed, so spam/score/reason are TOP-LEVEL keys.
        td.write(
            "20240101T000000_q1_spam.json",
            r#"{"queue_id":"q1","verdict":"spam","envelope_from":"spammer@evil.com",
                "outcomes":[{"model":"typesafe/jev-1.13","spam":true,"score":0.95,
                             "reason":"noul=0.950","error":null,"latency_ms":42,
                             "cost_usd":0.00012,"is_spam":true}]}"#,
        );
        td.write(
            "20240102T000000_q2_whitelisted.eml",
            "From: boss@example.com\r\nSubject: hi\r\n\r\nlegit",
        );
        td.write(
            "20240102T000000_q2_whitelisted.json",
            r#"{"queue_id":"q2","verdict":"whitelisted","envelope_from":"boss@example.com","outcomes":[]}"#,
        );
        td
    }

    #[test]
    fn html_escape_basic() {
        assert_eq!(html_escape("<b>&\"</b>"), "&lt;b&gt;&amp;&quot;&lt;/b&gt;");
    }

    #[test]
    fn query_parse_and_decode() {
        let q = parse_query("q=foo%40bar&limit=10&flag");
        assert_eq!(q.get("q").map(String::as_str), Some("foo@bar"));
        assert_eq!(q.get("limit").map(String::as_str), Some("10"));
        assert_eq!(q.get("flag").map(String::as_str), Some(""));
        // multi-byte UTF-8 survives percent-decoding (ä = %C3%A4)
        assert_eq!(decode_component("Gr%C3%BC%C3%9Fe+x", true), "Grüße x");
        // '+' is literal in paths, space only in queries
        assert_eq!(decode_component("a+b", false), "a+b");
        assert_eq!(url_encode("a b/x?"), "a%20b%2Fx%3F");
    }

    #[test]
    fn head_returns_headers_only() {
        let opts = DashboardOpts {
            stats: Stats::new(),
            spam_dir: PathBuf::from("/tmp/does-not-exist-xyz"),
            token: None,
            scramble_senders: false,
            cache: Arc::new(std::sync::Mutex::new(QuarantineCache::default())),
            eval_log: None,
            release_enabled: false,
        };
        let get = handle_request("GET /api/stats HTTP/1.1\r\n\r\n", &opts);
        let head = handle_request("HEAD /api/stats HTTP/1.1\r\n\r\n", &opts);
        assert!(get.windows(4).any(|w| w == b"\r\n\r\n"));
        let hdr_end = get.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(&head[..], &get[..hdr_end]); // same headers, no body
                                                // case-insensitive Authorization header
        let tok = Some("s".to_string());
        let q = parse_query("");
        assert!(authorized(
            "GET / HTTP/1.1\r\nAUTHORIZATION: Bearer s\r\n\r\n",
            &q,
            &tok
        ));
    }

    #[test]
    fn reject_path_traversal() {
        let dir = Path::new("/tmp/does-not-exist-xyz");
        assert!(read_mail(dir, "../evil").is_none());
        assert!(read_mail(dir, "a/b").is_none());
        assert!(read_mail(dir, "a\\b").is_none());
        assert!(read_mail(dir, "..").is_none());
    }

    #[test]
    fn cache_roundtrip_and_invalidation() {
        let td = quarantine_with_two_mails();
        // first pass populates the cache (load() sets the on-disk path)
        let mut c = QuarantineCache::load(td.path());
        let (_, total) = list_quarantine(td.path(), "", 100, 0, &mut c);
        assert_eq!(total, 2);
        assert_eq!(c.len(), 2);
        c.save();
        // cache file exists next to the data
        let cache_file = td.path().join(CACHE_FILE);
        assert!(cache_file.exists(), "cache persisted for restart");

        // a fresh cache loaded from disk serves the same data (restart-safe)
        let mut c2 = QuarantineCache::load(td.path());
        assert_eq!(c2.len(), 2);
        let (items, total) = list_quarantine(td.path(), "", 100, 0, &mut c2);
        assert_eq!(total, 2);
        assert!(
            (items[0].models[0].score - 0.95).abs() < 1e-6
                || items
                    .iter()
                    .any(|m| (m.models[0].score - 0.95).abs() < 1e-6)
        );

        // changed sidecar (new score) must invalidate the entry
        let before = items
            .iter()
            .find(|m| m.stem.contains("q1"))
            .unwrap()
            .clone();
        assert!((before.models[0].score - 0.95).abs() < 1e-6);
        td.write(
            "20240101T000000_q1_spam.json",
            r#"{"queue_id":"q1","verdict":"spam","envelope_from":"spammer@evil.com",
                "outcomes":[{"model":"typesafe/jev-1.13","spam":true,"score":0.42,
                             "error":null,"latency_ms":40,"is_spam":true}]}"#,
        );
        // bump mtime so the (stem,mtime,size) key changes
        let p = td.path().join("20240101T000000_q1_spam.json");
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        drop(f);
        let (items, _) = list_quarantine(td.path(), "", 100, 0, &mut c2);
        let after = items.iter().find(|m| m.stem.contains("q1")).unwrap();
        assert!(
            (after.models[0].score - 0.42).abs() < 1e-6,
            "score must refresh, got {}",
            after.models[0].score
        );
    }

    #[test]
    fn cache_version_mismatch_is_discarded() {
        let td = quarantine_with_two_mails();
        // fabricate a cache file from a future version
        std::fs::write(
            td.path().join(CACHE_FILE),
            r#"{"version":999999,"entries":{"x":{"mtime_unix":1,"size":1,
                "mail":{"stem":"x","mtime_unix":1,"size":1,"sender":"s",
                        "spam":null,"verdict":null,"summary":"","models":[]}}}}"#,
        )
        .unwrap();
        let mut c = QuarantineCache::load(td.path());
        assert!(c.is_empty(), "unknown version must start empty");
        // and it rebuilds from the real files
        let (_, total) = list_quarantine(td.path(), "", 100, 0, &mut c);
        assert_eq!(total, 2);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn cache_corrupt_file_is_ignored() {
        let td = quarantine_with_two_mails();
        std::fs::write(td.path().join(CACHE_FILE), "{not json at all").unwrap();
        let mut c = QuarantineCache::load(td.path());
        assert!(c.is_empty());
        let (_, total) = list_quarantine(td.path(), "", 100, 0, &mut c);
        assert_eq!(total, 2);
    }

    #[test]
    fn cache_prunes_deleted_mails() {
        let td = quarantine_with_two_mails();
        let mut c = QuarantineCache::load(td.path());
        list_quarantine(td.path(), "", 100, 0, &mut c);
        assert_eq!(c.len(), 2);
        // retention deleted one file
        std::fs::remove_file(td.path().join("20240101T000000_q1_spam.eml")).unwrap();
        let (_, total) = list_quarantine(td.path(), "", 100, 0, &mut c);
        assert_eq!(total, 1);
        assert_eq!(c.len(), 1, "stale entry dropped");
    }

    #[test]
    fn cache_survives_restart_with_scores() {
        // regression guard: the flattened sidecar score must round-trip
        // through the cache unchanged, not come back as 0.0
        let td = quarantine_with_two_mails();
        let mut c = QuarantineCache::load(td.path());
        list_quarantine(td.path(), "", 100, 0, &mut c);
        c.save();
        let c2 = QuarantineCache::load(td.path());
        let e = c2
            .entries
            .get("20240101T000000_q1_spam")
            .expect("entry cached");
        assert!(
            (e.mail.models[0].score - 0.95).abs() < 1e-6,
            "score lost through the cache: {}",
            e.mail.models[0].score
        );
    }

    #[test]
    fn sanitize_envelope_rejects_hostile_senders() {
        assert_eq!(sanitize_envelope("a@b.com").as_deref(), Some("a@b.com"));
        assert_eq!(sanitize_envelope("  a@b.com  ").as_deref(), Some("a@b.com"));
        // header injection attempts
        assert!(sanitize_envelope("a@b.com\nBcc: victim@x.com").is_none());
        assert!(sanitize_envelope("a@b.com\rX: y").is_none());
        assert!(sanitize_envelope("a@b.com -OQueueDirectory=/tmp").is_none()); // space splits
        assert!(sanitize_envelope("a@b.com\0").is_none());
        assert!(sanitize_envelope("").is_none());
        assert!(sanitize_envelope("<>").is_none());
        assert!(sanitize_envelope("notanaddress").is_none());
    }

    #[test]
    fn release_is_disabled_by_default() {
        let td = quarantine_with_two_mails();
        let mut opts = opts_for(td.path(), None, false);
        opts.release_enabled = false;
        let out = handle_request(
            "POST /mails/20240101T000000_q1_spam/release HTTP/1.1\r\n\r\n",
            &opts,
        );
        let body = String::from_utf8_lossy(&out);
        assert!(body.contains("403"), "release must be off by default");
    }

    #[test]
    fn release_route_requires_auth_and_valid_path() {
        let td = quarantine_with_two_mails();
        let mut opts = opts_for(td.path(), Some("t0k3n"), false);
        opts.release_enabled = true;
        // no token -> 401
        let out = handle_request("POST /mails/x_spam/release HTTP/1.1\r\n\r\n", &opts);
        assert!(String::from_utf8_lossy(&out).contains("401"));
        // wrong path -> 404 (query string belongs in the request line)
        let out = handle_request("POST /api/release?token=t0k3n HTTP/1.1\r\n\r\n", &opts);
        assert!(String::from_utf8_lossy(&out).contains("404"));
        // traversal in the stem -> 404
        let out = handle_request(
            "POST /mails/..%2f..%2fetc/release?token=t0k3n HTTP/1.1\r\n\r\n",
            &opts,
        );
        assert!(String::from_utf8_lossy(&out).contains("404"));
    }

    #[test]
    fn release_rejects_unusable_envelope() {
        let td = TempDir::new("release");
        td.write("bad.eml", "From: x@y\r\n\r\nbody");
        // empty envelope sender (a bounce) cannot be re-injected safely
        td.write(
            "bad.json",
            r#"{"verdict":"spam","envelope_from":"","outcomes":[]}"#,
        );
        let err = release_mail(td.path(), "bad", None).unwrap_err();
        assert!(matches!(err, ReleaseError::Invalid(_)), "got {err:?}");

        // unknown mail
        let err = release_mail(td.path(), "nope", None).unwrap_err();
        assert!(matches!(err, ReleaseError::Invalid(_)));
    }

    #[test]
    fn release_delivers_via_sendmail_and_audits() {
        let td = quarantine_with_two_mails();
        // fake sendmail that captures its stdin and argv
        let shim = td.path().join("fake-sendmail");
        let captured = td.path().join("captured.eml");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\ncat > '{}'\nprintf '%s\\n' \"$@\" > '{}'\n",
                captured.display(),
                shim.with_file_name("args.txt").display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev = std::env::var("MILTER_SENDMAIL_CMD").ok();
        std::env::set_var("MILTER_SENDMAIL_CMD", &shim);

        let log = td.path().join("eval.jsonl");
        let from = release_mail(td.path(), "20240101T000000_q1_spam", Some(log.as_path()))
            .expect("release");

        assert_eq!(from, "spammer@evil.com");
        let delivered = std::fs::read_to_string(&captured).unwrap();
        // release marker header is prepended
        assert!(
            delivered.starts_with("X-AIClassifier-Released:"),
            "missing release header: {}",
            delivered.lines().next().unwrap_or("")
        );
        // original mail survives intact
        assert!(delivered.contains("Subject: win"));
        assert!(delivered.contains("the body") || delivered.contains("body"));
        // argv: no shell metacharacters, sender passed as its own argument
        let args = std::fs::read_to_string(shim.with_file_name("args.txt")).unwrap();
        assert!(args.contains("spammer@evil.com"), "sender passed via -f");
        assert!(
            !args.contains(";") && !args.contains("$("),
            "no shell in argv"
        );
        // audit trail
        let trail = std::fs::read_to_string(&log).unwrap();
        assert!(trail.contains("\"event\":\"released\""));
        assert!(trail.contains("spammer@evil.com"));
        assert!(trail.contains("20240101T000000_q1_spam"));

        if let Some(p) = prev {
            std::env::set_var("MILTER_SENDMAIL_CMD", p);
        }
    }

    #[test]
    fn release_button_only_on_spam_and_only_when_enabled() {
        let td = quarantine_with_two_mails();
        let stem = "20240101T000000_q1_spam";
        let wl = "20240102T000000_q2_whitelisted";

        let mut off = opts_for(td.path(), None, false);
        off.release_enabled = false;
        let (_, body) = get(&off, "/mails");
        assert!(!body.contains("Deliver anyway"), "hidden when disabled");

        let mut on = opts_for(td.path(), None, false);
        on.release_enabled = true;
        let (_, body) = get(&on, "/mails");
        assert!(body.contains("Deliver anyway"));
        // POST form, never a GET link
        assert!(body.contains("method=post"));
        assert!(body.contains(&format!("/mails/{stem}/release")));
        assert!(body.contains("confirm("), "asks for confirmation");
        // not offered for the whitelisted (non-spam) entry
        assert!(!body.contains(&format!("/mails/{wl}/release")));
    }

    #[test]
    fn mail_source_view_copies_exactly() {
        // A mail with long lines (DKIM-ish) and CRLF: copying must not gain
        // soft-wrap newlines, and must not lose a single byte.
        let td = TempDir::new("src");
        let raw = format!(
            "From: a@b.example\r\nSubject: =?UTF-8?B?{}?=\r\nDKIM-Signature: v=1; a=rsa-sha256;\r\n\tverylongbas64={}\r\n\r\nbody line one\r\nbody line two\r\n",
            "w4ViYW4=",
            "A".repeat(300)
        );
        td.write("s.eml", &raw);
        td.write(
            "s.json",
            r#"{"verdict":"spam","envelope_from":"a@b.example","outcomes":[]}"#,
        );

        let opts = opts_for(td.path(), None, false);
        let (status, body) = get(&opts, "/mails/s");
        assert!(status.contains("200"));

        // container + copy affordances
        assert!(body.contains("id=mailsrc"), "source container");
        assert!(body.contains("class=body id=mailsrc"), "copyable body");
        assert!(body.contains("double-click to select"));
        assert!(body.contains("id=cpbtn"), "explicit copy button");

        // The copy button MUST read from /raw. Copying out of the rendered
        // HTML loses CRLF (the browser normalises line endings), which corrupts
        // a pasted mail — this assertion guards that mistake.
        let fetch_pos = body.find("await fetch('").expect("copy button uses fetch");
        let q = fetch_pos + "await fetch('".len();
        let fetched = &body[q..body[q..].find('\'').unwrap() + q];
        assert!(
            fetched.contains("/raw"),
            "copy must come from /raw, got {fetched}"
        );

        // /raw serves the exact bytes
        let (_, raw_resp) = get(&opts, "/mails/s/raw");
        assert_eq!(raw_resp, raw, "raw route must be byte-identical");

        // The rendered preview is for reading; long lines survive, but the
        // browser may re-flow line endings, so it is not a copy source.
        let s =
            body.find("<pre class=body id=mailsrc>").unwrap() + "<pre class=body id=mailsrc>".len();
        let e = body[s..].find("</pre>").unwrap() + s;
        let decoded = html_unescape_for_test(&body[s..e]);
        assert!(decoded.contains(&"A".repeat(300)), "long line intact");
    }

    #[test]
    fn mail_source_warns_when_truncated() {
        let td = TempDir::new("big");
        td.write("big.eml", &"x".repeat(PREVIEW_LIMIT + 10_000));
        td.write(
            "big.json",
            r#"{"verdict":"spam","envelope_from":"a@b","outcomes":[]}"#,
        );
        let opts = opts_for(td.path(), None, false);
        let (_, body) = get(&opts, "/mails/big");
        assert!(
            body.contains("Preview truncated"),
            "must warn about truncation"
        );
        // and the raw route still serves the whole thing
        let (_, raw) = get(&opts, "/mails/big/raw");
        assert_eq!(raw.len(), PREVIEW_LIMIT + 10_000, "raw is complete");
    }

    /// Inverse of html_escape for assertions only.
    fn html_unescape_for_test(s: &str) -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&")
    }

    #[test]
    fn list_quarantine_lists_newest_first_with_meta() {
        let td = quarantine_with_two_mails();
        let (items, total) =
            list_quarantine(td.path(), "", 100, 0, &mut QuarantineCache::default());
        assert_eq!(total, 2);
        // mtime ties are broken by stem, both files exist -> both returned
        assert_eq!(items.len(), 2);
        let spam = items.iter().find(|m| m.stem.contains("q1")).unwrap();
        assert_eq!(spam.sender, "spammer@evil.com");
        assert_eq!(spam.verdict.as_deref(), Some("spam"));
        assert_eq!(spam.spam, Some(true));
        assert_eq!(spam.models.len(), 1);
        assert_eq!(spam.models[0].name, "jev-1.13"); // shortened from typesafe/jev-1.13
        assert!(spam.models[0].spam);
        // score must survive the flatten -> it is a top-level key, not nested
        assert!(
            (spam.models[0].score - 0.95).abs() < 1e-6,
            "score parsed from flattened sidecar, got {}",
            spam.models[0].score
        );
        assert!(spam.summary.contains("SPAM") || spam.summary.contains("spam"));

        let wl = items.iter().find(|m| m.stem.contains("q2")).unwrap();
        assert_eq!(wl.verdict.as_deref(), Some("whitelisted"));
        assert_eq!(wl.spam, None, "no outcomes -> no derived spam flag");
        assert!(wl.models.is_empty());
    }

    #[test]
    fn list_quarantine_search_and_paging() {
        let td = quarantine_with_two_mails();
        // case-insensitive sender / stem search
        let (items, total) =
            list_quarantine(td.path(), "EVIL", 100, 0, &mut QuarantineCache::default());
        assert_eq!(total, 1);
        assert!(items[0].stem.contains("q1"));
        assert_eq!(
            list_quarantine(
                td.path(),
                "nomatch",
                100,
                0,
                &mut QuarantineCache::default()
            )
            .1,
            0
        );
        // paging
        let (page, total) = list_quarantine(td.path(), "", 1, 1, &mut QuarantineCache::default());
        assert_eq!(total, 2);
        assert_eq!(page.len(), 1);
        // limit is capped at 500
        assert_eq!(
            list_quarantine(td.path(), "", 10_000, 0, &mut QuarantineCache::default())
                .0
                .len(),
            2
        );
        // non-eml files are ignored
        td.write("notes.txt", "ignore me");
        assert_eq!(
            list_quarantine(td.path(), "", 100, 0, &mut QuarantineCache::default()).1,
            2
        );
        assert_eq!(quarantine_count(td.path()), 2);
    }

    #[test]
    fn sidecar_json_edge_cases() {
        let td = TempDir::new("sidecar");
        // missing file
        assert_eq!(read_sidecar_json(td.path(), "nope").0, "");
        // invalid json
        td.write("bad.json", "{not json");
        let (sender, spam, verdict, _summary, models) = read_sidecar_json(td.path(), "bad");
        assert!(sender.is_empty() && spam.is_none() && verdict.is_none() && models.is_empty());
        // error outcome renders as err chip
        td.write(
            "err.json",
            r#"{"envelope_from":"a@b","outcomes":[{"model":"m1","is_spam":false,
                "error":"timeout","latency_ms":5000}]}"#,
        );
        let (_, _, _, summary, models) = read_sidecar_json(td.path(), "err");
        assert!(models[0].err);
        assert!(summary.contains("err"));
    }

    #[test]
    fn read_mail_returns_meta_and_body() {
        let td = quarantine_with_two_mails();
        let (meta, eml) = read_mail(td.path(), "20240101T000000_q1_spam").unwrap();
        assert!(meta.contains("spammer@evil.com"));
        assert!(eml.contains("Subject: win"));
        assert!(read_mail(td.path(), "does-not-exist").is_none());
    }

    #[test]
    fn route_mails_lists_quarantine() {
        let td = quarantine_with_two_mails();
        let opts = opts_for(td.path(), None, false);
        let (status, body) = get(&opts, "/mails");
        assert!(status.contains("200"));
        assert!(body.contains("spammer@evil.com"));
        assert!(body.contains("boss@example.com"));
        assert!(body.contains("badge s"), "spam badge");
        assert!(body.contains("badge w"), "whitelisted badge");
        assert!(body.contains("chip"), "model chips rendered");
        assert!(body.contains("Search (sender/filename)"));
        // link to detail view
        assert!(body.contains("/mails/20240101T000000_q1_spam"));
    }

    #[test]
    fn route_mails_search_and_paging_html() {
        let td = quarantine_with_two_mails();
        let opts = opts_for(td.path(), None, false);
        let (status, body) = get(&opts, "/mails?q=evil");
        assert!(status.contains("200"));
        assert!(body.contains("spammer@evil.com"));
        assert!(!body.contains("boss@example.com"), "filtered out");
        assert!(body.contains("1 matches"));
        // paging link appears when more remain
        let (_, body) = get(&opts, "/mails?limit=1");
        assert!(body.contains("next"));
        // trailing slash works
        assert!(get(&opts, "/mails/").0.contains("200"));
    }

    #[test]
    fn route_mails_detail_and_raw() {
        let td = quarantine_with_two_mails();
        let opts = opts_for(td.path(), None, false);
        let (status, body) = get(&opts, "/mails/20240101T000000_q1_spam");
        assert!(status.contains("200"));
        assert!(body.contains("Subject: win"), "mail body preview");
        assert!(body.contains("spammer@evil.com"), "meta json");
        assert!(body.contains("raw .eml"));
        // raw endpoint returns the eml
        let (status, body) = get(&opts, "/mails/20240101T000000_q1_spam/raw");
        assert!(status.contains("200"));
        assert!(body.starts_with("From: spammer@evil.com"));
        // missing mail -> 404
        assert!(get(&opts, "/mails/nope").0.contains("404"));
        assert!(get(&opts, "/mails/nope/raw").0.contains("404"));
    }

    #[test]
    fn route_api_mails_json() {
        let td = quarantine_with_two_mails();
        let opts = opts_for(td.path(), None, false);
        let (status, body) = get(&opts, "/api/mails");
        assert!(status.contains("200"));
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(v["total"], 2);
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
        let item = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["stem"] == "20240101T000000_q1_spam")
            .unwrap();
        assert_eq!(item["verdict"], "spam");
        assert_eq!(item["models"][0]["name"], "jev-1.13");
        assert_eq!(item["models"][0]["spam"], true);
        // detail json
        let (status, body) = get(&opts, "/api/mails/20240101T000000_q1_spam");
        assert!(status.contains("200"));
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert!(v["meta"]["verdict"] == "spam");
        assert!(v["eml"].as_str().unwrap().contains("Subject: win"));
        assert!(get(&opts, "/api/mails/nope").0.contains("404"));
    }

    #[test]
    fn route_errors_and_methods() {
        let td = TempDir::new("routes");
        let opts = opts_for(td.path(), None, false);
        // unknown route
        assert!(get(&opts, "/nope").0.contains("404"));
        assert!(get(&opts, "/api/nope").0.contains("404"));
        // POST is only routed for the release action; anything else still 405s
        let out = handle_request("POST /api/stats HTTP/1.1\r\n\r\n", &opts);
        assert!(String::from_utf8_lossy(&out).contains("404"));
        let out = handle_request("DELETE / HTTP/1.1\r\n\r\n", &opts);
        assert!(String::from_utf8_lossy(&out).contains("405"));
        // "/mails/" is the list route (200), not a mail detail lookup
        assert!(get(&opts, "/mails/").0.contains("200"));
        // nested path under a mail detail is rejected
        assert!(get(&opts, "/mails/a/b").0.contains("404"));
        // empty quarantine dir still renders
        let (status, body) = get(&opts, "/mails");
        assert!(status.contains("200"));
        assert!(body.contains("0 matches"));
        // missing dir -> treated as empty, not an error
        let missing = opts_for(Path::new("/tmp/does-not-exist-xyz"), None, false);
        assert!(get(&missing, "/mails").0.contains("200"));
    }

    #[test]
    fn auth_gates_every_route() {
        let td = quarantine_with_two_mails();
        let opts = opts_for(td.path(), Some("t0k3n"), false);
        for path in [
            "/",
            "/mails",
            "/api/stats",
            "/api/mails",
            "/mails/20240101T000000_q1_spam",
        ] {
            assert!(get(&opts, path).0.contains("401"), "{path} must be gated");
        }
        // correct token passes
        assert!(get(&opts, "/?token=t0k3n").0.contains("200"));
        // wrong token fails
        assert!(get(&opts, "/?token=wrong").0.contains("401"));
    }

    #[test]
    fn scramble_masks_senders_in_html_and_json() {
        let td = quarantine_with_two_mails();
        let opts = opts_for(td.path(), None, true);

        let (_, body) = get(&opts, "/mails");
        assert!(body.contains("class=\"scr\""), "senders blurred");
        assert!(body.contains("Scramble: on"));
        // search input also blurred
        assert!(body.contains("value=\"\" class=\"scr\""));

        // detail view redacts the sender inside the <pre> blocks
        let (_, body) = get(&opts, "/mails/20240101T000000_q1_spam");
        assert!(
            !body.contains("spammer@evil.com"),
            "sender redacted in detail"
        );
        assert!(body.contains("•••@•••"));
        assert!(body.contains("Subject: win"), "rest of the mail intact");

        // json api masks senders
        let (_, body) = get(&opts, "/api/mails");
        assert!(!body.contains("spammer@evil.com"));
        assert!(body.contains("•••@•••"));
        // raw download is intentionally NOT redacted (explicit user action)
        let (_, body) = get(&opts, "/mails/20240101T000000_q1_spam/raw");
        assert!(body.contains("spammer@evil.com"));
    }

    #[test]
    fn scramble_off_shows_plain_senders() {
        let td = quarantine_with_two_mails();
        let opts = opts_for(td.path(), None, false);
        let (_, body) = get(&opts, "/mails");
        assert!(!body.contains("class=\"scr\""));
        assert!(body.contains("Scramble: off"));
        let (_, body) = get(&opts, "/mails/20240101T000000_q1_spam");
        assert!(body.contains("spammer@evil.com"));
    }

    #[test]
    fn nav_preserves_token_across_pages() {
        let td = quarantine_with_two_mails();
        let opts = opts_for(td.path(), Some("t0k3n"), false);
        // nav on the index keeps the token so navigation doesn't 401
        let (_, body) = get(&opts, "/?token=t0k3n");
        assert!(body.contains("/mails?token=t0k3n"));
        assert!(body.contains("/api/stats?token=t0k3n"));
        // search form carries it as a hidden field
        let (_, body) = get(&opts, "/mails?token=t0k3n");
        assert!(body.contains("<input type=hidden name=token value=\"t0k3n\">"));
        // and the detail page too
        let (_, body) = get(&opts, "/mails/20240101T000000_q1_spam?token=t0k3n");
        assert!(body.contains("/mails?token=t0k3n"));
    }

    #[test]
    fn html_output_is_escaped() {
        let td = TempDir::new("xss");
        // sender with HTML chars must not break out of the table cell
        td.write("x.eml", "From: a@b\r\n\r\nbody");
        td.write(
            "x.json",
            r#"{"verdict":"spam","envelope_from":"<script>alert(1)</script>","outcomes":[]}"#,
        );
        let opts = opts_for(td.path(), None, false);
        let (_, body) = get(&opts, "/mails");
        assert!(!body.contains("<script>alert(1)</script>"));
        assert!(body.contains("&lt;script&gt;"));
        // detail: raw .eml body is escaped too
        let (_, body) = get(&opts, "/mails/x");
        assert!(!body.contains("<script>alert(1)</script>"));
    }

    #[test]
    fn stats_record_counts() {
        let s = Stats::new();
        s.record(
            RecentEntry {
                ts: "t".into(),
                queue_id: "q".into(),
                sender: "a@x".into(),
                spam: true,
                whitelisted: false,
                temperror: false,
                summary: "s".into(),
                models: Vec::new(),
            },
            &[("m".into(), true, 0.9, 10, Some(0.001), false)],
        );
        let v = s.snapshot_json(0);
        assert_eq!(v["total"], 1);
        assert_eq!(v["spam"], 1);
        assert_eq!(v["per_model"]["m"]["ok"], 1);
    }

    #[test]
    fn auth_open_without_token() {
        let q = parse_query("");
        assert!(authorized("GET / HTTP/1.1", &q, &None));
    }

    #[test]
    fn auth_token_required() {
        let q = parse_query("");
        let tok = Some("secret".to_string());
        assert!(!authorized("GET / HTTP/1.1", &q, &tok));
        let q2 = parse_query("token=secret");
        assert!(authorized("GET /?token=secret HTTP/1.1", &q2, &tok));
        assert!(authorized(
            "GET / HTTP/1.1\nAuthorization: Bearer secret",
            &q,
            &tok
        ));
    }

    #[test]
    fn href_q_keeps_and_overrides() {
        let q = parse_query("token=s3cret&scramble=1");
        assert_eq!(href_q("/mails", &q, &[]), "/mails?token=s3cret&scramble=1");
        assert_eq!(
            href_q("/mails", &q, &[("scramble", "0")]),
            "/mails?token=s3cret&scramble=0"
        );
        assert_eq!(
            href_q(
                "/mails",
                &parse_query("token=a%20b"),
                &[("q", "x@y"), ("offset", "10")]
            ),
            "/mails?token=a%20b&q=x%40y&offset=10"
        );
        assert_eq!(href_q("/mails", &parse_query(""), &[]), "/mails");
    }

    #[test]
    fn scramble_effective_and_rendered() {
        assert!(eff_scramble(&parse_query("scramble=1"), false));
        assert!(!eff_scramble(&parse_query("scramble=0"), true));
        assert!(eff_scramble(&parse_query(""), true));
        assert!(!eff_scramble(&parse_query("scramble=bogus"), false));
        assert_eq!(
            redact_sender("From: a@x, cc a@x", "a@x"),
            "From: •••@•••, cc •••@•••"
        );
        let opts = DashboardOpts {
            stats: Stats::new(),
            spam_dir: PathBuf::from("/tmp/does-not-exist-xyz"),
            token: None,
            scramble_senders: false,
            cache: Arc::new(std::sync::Mutex::new(QuarantineCache::default())),
            eval_log: None,
            release_enabled: false,
        };
        let raw = handle_request("GET / HTTP/1.1\r\n\r\n", &opts);
        let plain = String::from_utf8_lossy(&raw);
        assert!(plain.contains("const SCRAMBLE=false;"));
        assert!(plain.contains("Scramble: off"));
        let raw_scr = handle_request("GET /?scramble=1 HTTP/1.1\r\n\r\n", &opts);
        let scr = String::from_utf8_lossy(&raw_scr);
        assert!(scr.contains("Scramble: on"));
        assert!(scr.contains("const SCRAMBLE=true;"));
    }

    #[test]
    fn sidecar_verdict_badge() {
        let wl = QuarantineMail {
            stem: "x".into(),
            mtime_unix: 0,
            size: 0,
            sender: String::new(),
            spam: None,
            verdict: Some("whitelisted".into()),
            summary: String::new(),
            models: Vec::new(),
        };
        assert!(mail_badge(&wl).contains("whitelisted"));
        let legacy_spam = QuarantineMail {
            stem: "x".into(),
            mtime_unix: 0,
            size: 0,
            sender: String::new(),
            spam: Some(true),
            verdict: None,
            summary: String::new(),
            models: Vec::new(),
        };
        assert!(mail_badge(&legacy_spam).contains("SPAM"));
        // stored verdict wins over derived flag
        let mixed = QuarantineMail {
            stem: "x".into(),
            mtime_unix: 0,
            size: 0,
            sender: String::new(),
            spam: Some(true),
            verdict: Some("ham".into()),
            summary: String::new(),
            models: Vec::new(),
        };
        assert!(mail_badge(&mixed).contains(">ham<"));
    }

    #[test]
    fn chips_rendering() {
        assert_eq!(short_model("typesafe/jev-1.13"), "jev-1.13");
        assert_eq!(short_model("plain"), "plain");
        let chips = vec![
            ModelChip {
                name: "jev-1.13".into(),
                spam: true,
                err: false,
                score: 0.956,
            },
            ModelChip {
                name: "clef-omni".into(),
                spam: false,
                err: false,
                score: 0.1,
            },
            ModelChip {
                name: "m3".into(),
                spam: false,
                err: true,
                score: 0.0,
            },
        ];
        let h = chips_html(&chips);
        assert!(h.contains("class=\"chip s\""));
        assert!(h.contains("jev-1.13 · SPAM 0.96"));
        assert!(h.contains("class=\"chip h\""));
        assert!(h.contains("clef-omni · ham 0.10"));
        assert!(h.contains("m3 · err"));
        // record() attaches chips to the entry
        let s = Stats::new();
        s.record(
            RecentEntry {
                ts: "t".into(),
                queue_id: "q".into(),
                sender: "a@x".into(),
                spam: true,
                whitelisted: false,
                temperror: false,
                summary: "s".into(),
                models: Vec::new(),
            },
            &[("org/jev-1.13".into(), true, 0.9, 10, None, false)],
        );
        let v = s.snapshot_json(0);
        assert_eq!(v["recent"][0]["models"][0]["name"], "jev-1.13");
        assert_eq!(v["recent"][0]["models"][0]["spam"], true);
    }

    #[test]
    fn index_page_is_live() {
        let opts = DashboardOpts {
            stats: Stats::new(),
            spam_dir: PathBuf::from("/tmp/does-not-exist-xyz"),
            token: None,
            scramble_senders: false,
            cache: Arc::new(std::sync::Mutex::new(QuarantineCache::default())),
            eval_log: None,
            release_enabled: false,
        };
        let out = handle_request("GET / HTTP/1.1\r\n\r\n", &opts);
        let body = String::from_utf8_lossy(&out);
        assert!(body.contains("id=\"c-total\""));
        assert!(body.contains("id=\"m-models\""));
        assert!(body.contains("setInterval(tick, 3000)"));
        // with token auth, nav keeps ?token=
        let opts_t = DashboardOpts {
            stats: Stats::new(),
            spam_dir: PathBuf::from("/tmp/does-not-exist-xyz"),
            token: Some("s".into()),
            scramble_senders: false,
            cache: Arc::new(std::sync::Mutex::new(QuarantineCache::default())),
            eval_log: None,
            release_enabled: false,
        };
        let out = handle_request("GET /?token=s HTTP/1.1\r\n\r\n", &opts_t);
        let body = String::from_utf8_lossy(&out);
        assert!(body.contains("/mails?token=s"));
    }
}
