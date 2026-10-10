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

    /// Outcome row as recorded: (model, is_spam, score, latency_ms, cost, error?).
    pub fn record(&self, entry: RecentEntry, models: &[(String, bool, f32, u64, Option<f64>, bool)]) {
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
                let avg_score = if m.ok > 0 { m.score_sum / m.ok as f64 } else { 0.0 };
                let avg_lat = if m.ok > 0 { m.latency_ms_sum as f64 / m.ok as f64 } else { 0.0 };
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
                "summary": r.summary,
            })).collect::<Vec<_>>(),
        })
    }

    fn recent_html(&self) -> String {
        let g = match self.inner.read() {
            Ok(g) => g,
            Err(_) => return String::new(),
        };
        let mut h = String::from(
            "<table><tr><th>Zeit</th><th>Queue</th><th>Sender</th><th>Verdict</th><th>Summary</th></tr><tbody id=\"m-recent\">",
        );
        if g.recent.is_empty() {
            h.push_str("<tr><td colspan=5><i>Noch keine Mails seit Start.</i></td></tr>");
        }
        for r in g.recent.iter().take(20) {
            h.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                html_escape(&r.ts),
                html_escape(&r.queue_id),
                html_escape(&r.sender),
                verdict_badge(r.whitelisted, r.temperror, r.spam),
                html_escape(&r.summary)
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
            <div class=card><b id=\"c-q\">{quarantine_count}</b><span>quarantäne-dateien</span></div>\
            </div>"
        )
    }

    fn per_model_html(&self) -> String {
        let rows: Vec<(String, ModelStats)> = match self.inner.read() {
            Ok(g) => g.per_model.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            Err(_) => Vec::new(),
        };
        let mut h = String::from(
            "<h2>Modelle</h2><table><tr><th>Modell</th><th>ok</th><th>spam</th><th>err</th><th>avg score</th><th>avg lat</th><th>kosten $</th></tr><tbody id=\"m-models\">",
        );
        if rows.is_empty() {
            h.push_str("<tr><td colspan=7><i>keine Daten</i></td></tr>");
        }
        let mut rows = rows;
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, m) in rows {
            let avg_s = if m.ok > 0 { m.score_sum / m.ok as f64 } else { 0.0 };
            let avg_l = if m.ok > 0 { m.latency_ms_sum as f64 / m.ok as f64 } else { 0.0 };
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

#[derive(Debug, Clone)]
pub struct QuarantineMail {
    pub stem: String,
    pub mtime_unix: u64,
    pub size: u64,
    pub sender: String,
    pub spam: Option<bool>,
    pub summary: String,
}

pub fn quarantine_count(dir: &Path) -> usize {
    std::fs::read_dir(dir).map(|rd| rd.filter(|e| is_eml(e)).count()).unwrap_or(0)
}

fn is_eml(e: &std::io::Result<std::fs::DirEntry>) -> bool {
    e.as_ref().map(|e| e.path().extension().map(|x| x.eq_ignore_ascii_case("eml")).unwrap_or(false)).unwrap_or(false)
}

/// List quarantined mails, newest first. `q` filters stem/sender substring (case-insensitive).
pub fn list_quarantine(dir: &Path, q: &str, limit: usize, offset: usize) -> (Vec<QuarantineMail>, usize) {
    let mut all: Vec<QuarantineMail> = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return (Vec::new(), 0),
    };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().map(|x| !x.eq_ignore_ascii_case("eml")).unwrap_or(true) {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        if stem.is_empty() {
            continue;
        }
        let (mtime_unix, size) = e
            .metadata()
            .map(|m| {
                let t = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
                (t, m.len())
            })
            .unwrap_or((0, 0));
        let (sender, spam, summary) = read_sidecar_json(dir, &stem);
        all.push(QuarantineMail { stem, mtime_unix, size, sender, spam, summary });
    }
    all.sort_by(|a, b| b.mtime_unix.cmp(&a.mtime_unix).then(a.stem.cmp(&b.stem)));
    if !q.is_empty() {
        let ql = q.to_lowercase();
        all.retain(|m| m.stem.to_lowercase().contains(&ql) || m.sender.to_lowercase().contains(&ql));
    }
    let total = all.len();
    let items = all.into_iter().skip(offset).take(limit.min(500)).collect();
    (items, total)
}

fn read_sidecar_json(dir: &Path, stem: &str) -> (String, Option<bool>, String) {
    let p = dir.join(format!("{stem}.json"));
    let Ok(s) = std::fs::read_to_string(&p) else { return (String::new(), None, String::new()) };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) else { return (String::new(), None, String::new()) };
    let sender = v.get("envelope_from").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let outcomes = v.get("outcomes").and_then(|x| x.as_array()).cloned().unwrap_or_default();
    // Compact summary like the X-LLM-Verdict tag.
    let mut parts: Vec<String> = Vec::new();
    for o in outcomes.iter().take(6) {
        let model = o.get("model").and_then(|x| x.as_str()).unwrap_or("?");
        let short: String = model.split('/').next_back().unwrap_or(model).chars().take(14).collect();
        if o.get("error").and_then(|x| x.as_str()).is_some() {
            parts.push(format!("{short}=err"));
        } else {
            let spam = o.get("is_spam").and_then(|x| x.as_bool()).unwrap_or(false);
            let score = o.get("verdict").and_then(|x| x.get("score")).and_then(|x| x.as_f64()).unwrap_or(0.0);
            parts.push(format!("{short}={}:{score:.2}", if spam { "spam" } else { "ham" }));
        }
    }
    let summary: String = parts.join(" | ").chars().take(160).collect();
    // spam flag is not stored in the sidecar; derive from outcomes when present.
    let spam = if outcomes.is_empty() { None } else { Some(outcomes.iter().any(|o| o.get("is_spam").and_then(|x| x.as_bool()).unwrap_or(false))) };
    (sender, spam, summary)
}

pub fn read_mail(dir: &Path, stem: &str) -> Option<(String, String)> {
    if stem.contains('/') || stem.contains('\\') || stem.contains("..") {
        return None;
    }
    let eml = std::fs::read(dir.join(format!("{stem}.eml"))).ok()?;
    let meta = std::fs::read_to_string(dir.join(format!("{stem}.json"))).unwrap_or_else(|_| "{}".into());
    // Cap preview at ~200KB so a huge mail doesn't blow up the page.
    let text = String::from_utf8_lossy(&eml[..eml.len().min(200_000)]).into_owned();
    Some((meta, text))
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
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
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
form{display:flex;gap:.5rem;margin:1rem 0;flex-wrap:wrap}\
input[type=text]{padding:.45rem .6rem;width:300px;border:1px solid #cbd5e1;border-radius:8px}\
button{padding:.45rem 1rem;border:none;border-radius:8px;background:#1d4ed8;color:#fff;font-weight:600;cursor:pointer}\
button:hover{background:#1e40af}\
.pager{margin:1rem 0}";

/// Live refresh for the overview page: polls /api/stats every 3s and updates
/// cards + tables in place (plain HTTP fetch, no WebSocket/SSE needed).
const LIVE_JS: &str = r#"<script>
const tok = new URLSearchParams(location.search).get('token') || '';
const api = tok ? '/api/stats?token=' + encodeURIComponent(tok) : '/api/stats';
const esc = s => String(s == null ? '' : s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const badge = (t, c) => '<span class="badge ' + c + '">' + esc(t) + '</span>';
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
      || '<tr><td colspan="7"><i>keine Daten</i></td></tr>';
    document.getElementById('m-recent').innerHTML = (d.recent || []).slice(0, 20).map(x => {
      const v = x.whitelisted ? badge('whitelisted', 'w') : x.temperror ? badge('temperror', 't')
        : x.spam ? badge('SPAM', 's') : badge('ham', 'h');
      return '<tr><td>' + esc(x.ts) + '</td><td>' + esc(x.queue_id) + '</td><td>' + esc(x.sender) +
        '</td><td>' + v + '</td><td>' + esc(x.summary) + '</td></tr>';
    }).join('') || '<tr><td colspan="5"><i>Noch keine Mails seit Start.</i></td></tr>';
    set('m-updated', 'live · aktualisiert ' + new Date().toLocaleTimeString());
    if (dot) { dot.textContent = '●'; dot.className = 'dot on'; }
  } catch (e) {
    if (dot) { dot.textContent = '●'; dot.className = 'dot off'; }
    set('m-updated', 'offline — versuche erneut …');
  }
}
tick(); setInterval(tick, 3000);
</script>"#;

/// Raw token from the query so generated links keep working under token auth.
fn qtoken(query: &HashMap<String, String>) -> &str {
    query.get("token").map(String::as_str).unwrap_or("")
}

/// Internal link builder, preserving ?token= when present.
fn href(base: &str, tok: &str, params: &[(&str, &str)]) -> String {
    let mut q = String::new();
    if !tok.is_empty() {
        q.push_str(&format!("token={}", url_encode(tok)));
    }
    for (k, v) in params {
        if !q.is_empty() {
            q.push('&');
        }
        q.push_str(&format!("{k}={}", url_encode(v)));
    }
    if q.is_empty() { base.to_string() } else { format!("{base}?{q}") }
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

fn page(title: &str, body: &str, tok: &str, script: &str) -> String {
    let nav = format!(
        "<nav class=top><a href=\"{}\">Stats</a><a href=\"{}\">Spam-Mails</a><span class=sp></span><a href=\"{}\">JSON</a></nav>",
        href("/", tok, &[]),
        href("/mails", tok, &[]),
        href("/api/stats", tok, &[]),
    );
    format!(
        "<!doctype html><html lang=de><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
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
    let head_only = req.lines().next().unwrap_or("").split_whitespace().next() == Some("HEAD");
    let mut out = handle_get(req, opts);
    if head_only {
        if let Some(hdr_end) = out.windows(4).position(|w| w == b"\r\n\r\n") {
            out.truncate(hdr_end + 4);
        }
    }
    out
}

fn handle_get(req: &str, opts: &DashboardOpts) -> Vec<u8> {
    let mut lines = req.lines();
    let request_line = lines.next().unwrap_or("").trim_end_matches('\r').to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    if method != "GET" && method != "HEAD" {
        return resp("405 Method Not Allowed", "text/plain", b"only GET supported");
    }
    let (path, qs) = match target.split_once('?') { Some((p, q)) => (p, q), None => (target, "") };
    let query = parse_query(qs);
    if !authorized(req, &query, &opts.token) {
        return resp("401 Unauthorized", "text/plain", b"missing or wrong token");
    }
    let path = decode_component(path, false);
    tracing::debug!(method = method, path = %path, "dashboard request");


    if path == "/" {
        let qc = quarantine_count(&opts.spam_dir);
        let mut body = opts.stats.overview_cards(qc);
        body.push_str(&opts.stats.per_model_html());
        body.push_str("<h2>Letzte Mails</h2>");
        body.push_str(&opts.stats.recent_html());
        body.push_str(&format!("<p><a href=\"{}\">Spam-Quarantäne browsen &rarr;</a></p>", href("/mails", qtoken(&query), &[])));
        return resp("200 OK", "text/html; charset=utf-8", page("Milter-Stats", &body, qtoken(&query), LIVE_JS).as_bytes());
    }
    if path == "/api/stats" {
        let qc = quarantine_count(&opts.spam_dir);
        let body = serde_json::to_string(&opts.stats.snapshot_json(qc)).unwrap_or_default();
        return resp("200 OK", "application/json", body.as_bytes());
    }
    if path == "/mails" || path == "/mails/" || path == "/api/mails" {
        let q = query.get("q").cloned().unwrap_or_default();
        let limit = parse_usize(query.get("limit"), 100).min(500);
        let offset = parse_usize(query.get("offset"), 0);
        let (items, total) = list_quarantine(&opts.spam_dir, &q, limit, offset);
        if path.starts_with("/api") {
            let body = serde_json::to_string(&serde_json::json!({
                "total": total, "limit": limit, "offset": offset,
                "items": items.iter().map(|m| serde_json::json!({
                    "stem": m.stem, "mtime_unix": m.mtime_unix, "size": m.size,
                    "sender": m.sender, "spam": m.spam, "summary": m.summary,
                })).collect::<Vec<_>>(),
            }))
            .unwrap_or_default();
            return resp("200 OK", "application/json", body.as_bytes());
        }
        let mut body = format!(
            "<form method=get action=\"{}\"><input type=text name=q placeholder=\"Suche (Sender/Dateiname)\" value=\"{}\">{}\
            <button>Suchen</button></form><p>{} Treffer (zeige {} ab {})</p>",
            href("/mails", "", &[]),
            html_escape(&q),
            if qtoken(&query).is_empty() { String::new() } else {
                format!("<input type=hidden name=token value=\"{}\">", html_escape(qtoken(&query)))
            },
            total,
            items.len(),
            offset
        );
        body.push_str("<table><tr><th>Datum</th><th>Datei</th><th>Sender</th><th>Verdict</th><th>Modelle</th></tr>");
        for m in &items {
            let dt = chrono::DateTime::from_timestamp(m.mtime_unix as i64, 0)
                .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "-".into());
            let verdict = match m.spam { Some(true) => "<span class=\"badge s\">SPAM</span>", Some(false) => "<span class=\"badge h\">ham</span>", None => "-" };
            body.push_str(&format!(
                "<tr><td>{dt}</td><td><a href=\"{}\">{}</a> ({} KB)</td><td>{}</td><td>{verdict}</td><td>{}</td></tr>",
                href(&format!("/mails/{}", url_encode(&m.stem)), qtoken(&query), &[]),
                html_escape(&m.stem),
                m.size / 1024,
                html_escape(&m.sender),
                html_escape(&m.summary)
            ));
        }
        body.push_str("</table>");
        if total > offset + items.len() {
            body.push_str(&format!(
                "<p class=pager><a href=\"{}\">nächste &rarr;</a></p>",
                href("/mails", qtoken(&query), &[("q", &q), ("limit", &limit.to_string()), ("offset", &(offset + items.len()).to_string())]),
            ));
        }
        return resp("200 OK", "text/html; charset=utf-8", page("Spam-Quarantäne", &body, qtoken(&query), "").as_bytes());
    }
    if let Some(stem) = path.strip_prefix("/mails/") {
        let (stem, raw) = match stem.strip_suffix("/raw") {
            Some(s) => (s.to_string(), true),
            None => (stem.to_string(), false),
        };
        if stem.is_empty() || stem.contains('/') {
            return resp("404 Not Found", "text/plain", b"not found");
        }
        match read_mail(&opts.spam_dir, &stem) {
            Some((_meta, eml)) if raw => {
                return resp("200 OK", "text/plain; charset=utf-8", eml.as_bytes());
            }
            Some((meta, eml)) => {
                let pretty = serde_json::from_str::<serde_json::Value>(&meta)
                    .map(|v| serde_json::to_string_pretty(&v).unwrap_or(meta.clone()))
                    .unwrap_or(meta);
                let body = format!(
                    "<p><a href=\"{}\">&larr; zurück</a> &middot; <a href=\"{}\">raw .eml</a></p>\
                    <h2>Meta</h2><pre>{}</pre><h2>Mail</h2><pre>{}</pre>",
                    href("/mails", qtoken(&query), &[]),
                    href(&format!("/mails/{}/raw", url_encode(&stem)), qtoken(&query), &[]),
                    html_escape(&pretty),
                    html_escape(&eml)
                );
                return resp("200 OK", "text/html; charset=utf-8", page(&stem, &body, qtoken(&query), "").as_bytes());
            }
            None => return resp("404 Not Found", "text/plain", b"mail not found"),
        }
    }
    if let Some(stem) = path.strip_prefix("/api/mails/") {
        if stem.is_empty() || stem.contains('/') {
            return resp("404 Not Found", "application/json", br#"{"error":"not found"}"#);
        }
        match read_mail(&opts.spam_dir, stem) {
            Some((meta, eml)) => {
                let meta_v: serde_json::Value = serde_json::from_str(&meta).unwrap_or(serde_json::Value::Null);
                let body = serde_json::to_string(&serde_json::json!({"stem": stem, "meta": meta_v, "eml": eml})).unwrap_or_default();
                return resp("200 OK", "application/json", body.as_bytes());
            }
            None => return resp("404 Not Found", "application/json", br#"{"error":"not found"}"#),
        }
    }
    resp("404 Not Found", "text/plain", b"not found")
}

pub async fn serve(
    listen: String,
    opts: DashboardOpts,
) -> std::io::Result<()> {
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
        let opts = DashboardOpts { stats: Stats::new(), spam_dir: PathBuf::from("/tmp/does-not-exist-xyz"), token: None };
        let get = handle_request("GET /api/stats HTTP/1.1\r\n\r\n", &opts);
        let head = handle_request("HEAD /api/stats HTTP/1.1\r\n\r\n", &opts);
        assert!(get.windows(4).any(|w| w == b"\r\n\r\n"));
        let hdr_end = get.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(&head[..], &get[..hdr_end]); // same headers, no body
        // case-insensitive Authorization header
        let tok = Some("s".to_string());
        let q = parse_query("");
        assert!(authorized("GET / HTTP/1.1\r\nAUTHORIZATION: Bearer s\r\n\r\n", &q, &tok));
    }

    #[test]
    fn reject_path_traversal() {
        let dir = Path::new("/tmp/does-not-exist-xyz");
        assert!(read_mail(dir, "../evil").is_none());
        assert!(read_mail(dir, "a/b").is_none());
    }

    #[test]
    fn stats_record_counts() {
        let s = Stats::new();
        s.record(
            RecentEntry { ts: "t".into(), queue_id: "q".into(), sender: "a@x".into(), spam: true, whitelisted: false, temperror: false, summary: "s".into() },
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
        assert!(authorized("GET / HTTP/1.1\nAuthorization: Bearer secret", &q, &tok));
    }

    #[test]
    fn href_preserves_token() {        assert_eq!(href("/mails", "", &[]), "/mails");
        assert_eq!(href("/mails", "s3cret", &[]), "/mails?token=s3cret");
        assert_eq!(
            href("/mails", "a b", &[("q", "x@y"), ("offset", "10")]),
            "/mails?token=a%20b&q=x%40y&offset=10"
        );
    }

    #[test]
    fn index_page_is_live() {
        let opts = DashboardOpts { stats: Stats::new(), spam_dir: PathBuf::from("/tmp/does-not-exist-xyz"), token: None };
        let out = handle_request("GET / HTTP/1.1\r\n\r\n", &opts);
        let body = String::from_utf8_lossy(&out);
        assert!(body.contains("id=\"c-total\""));
        assert!(body.contains("id=\"m-models\""));
        assert!(body.contains("setInterval(tick, 3000)"));
        // with token auth, nav keeps ?token=
        let opts_t = DashboardOpts { stats: Stats::new(), spam_dir: PathBuf::from("/tmp/does-not-exist-xyz"), token: Some("s".into()) };
        let out = handle_request("GET /?token=s HTTP/1.1\r\n\r\n", &opts_t);
        let body = String::from_utf8_lossy(&out);
        assert!(body.contains("/mails?token=s"));
    }
}
