# ai-classification-milter

Postfix pre-queue milter that asks **N decision models in parallel** whether a mail
is spam/phishing, then **tags or rejects** it — and never breaks mail delivery.

```
MAIL → RCPT* → HEADER* → BODY* → EOM
                              └→ N models in parallel (OpenRouter Decisions API / Noul)
                                   └→ X-AIClassifier-Verdict header + eval.jsonl + quarantine
```

* **Multi-model by design.** One verdict is a guess; five models plus a configurable
  decision rule (`any` / `majority` / `all`) give you something you can actually audit
  and tune against `eval.jsonl`.
* **Fail-open, always.** A model timeout, an API outage, or a dead process must not
  drop mail. Errors never count as spam, and Postfix accepts if the milter is gone.
* **Learn mode first.** Run in observation mode, tune the threshold and the question,
  then switch to rejecting.
* **Optional dashboard.** Live stats and a quarantine browser for screenshots and
  triage — off by default, no new dependencies.

---

## Table of contents

- [Quick start](#quick-start)
- [How it works](#how-it-works)
- [Fail-open guarantee](#fail-open-guarantee)
- [Deployment](#deployment) · [systemd](#systemd) · [Postfix](#postfix) · [Sendmail-only hosts](#sendmail-only-hosts)
- [Configuration](#configuration) · [Dashboard](#dashboard)
- [CLI mode](#cli-mode)
- [Operations](#operations) · [Upgrading](#upgrading) · [Troubleshooting](#troubleshooting)
- [Development](#development) · [Repo layout](#repo-layout)
- [Security & privacy](#security--privacy)

---

## Quick start

**1. Build**

```sh
cargo build --release           # or: just build-release
```

**2. Configure**

```sh
cp ai-classification-milter.toml.example ai-classification-milter.toml
$EDITOR ai-classification-milter.toml          # annotated, see [Configuration](#configuration)
export OPENROUTER_API_KEY=sk-or-...            # secret via env, never into the TOML
```

**3. Run**

```sh
./target/release/ai-classification-milter          # milter daemon (Postfix)
./target/release/ai-classification-milter -i < mail.eml   # one-off evaluation
```

**4. Wire it into Postfix** (`/etc/postfix/main.cf`) — see [Postfix](#postfix):

```
smtpd_milters = { inet:127.0.0.1:8891, connect_timeout=10s, content_timeout=30s, default_action=accept }
milter_protocol = 6
milter_default_action = accept
```

**5. Verify** — send a test mail and check for the header:

```sh
sendmail -v someone@example.com < test.eml
grep -i X-AIClassifier-Verdict /var/log/mail.log     # or check the delivered message headers
```

`X-AIClassifier-Verdict: jev-1.13=spam.96 | clef-omni=spam.90 | ...`

> **Start in `learn_mode = true` (default).** Nothing is rejected while you tune the
> question, threshold and model set. Flip to `false` when the numbers look right.

---

## How it works

At `EOM` (end of message) the milter builds one `state` object and fans it out to every
configured model **in parallel**, each with its own timeout:

| Input to the models | Content |
| --- | --- |
| Envelope | sender, recipients, client address, HELO |
| **All headers** | as an array under `email-headers`, duplicates (`Received` chains) preserved |
| Body | excerpt, first `model_body_chars` (default 4000) |

Each model answers **one Noul question** — `is_spam`: *"Is this mail spam, phishing or
malicious?"* — as a yes-probability `0.0–1.0`. No free-text parsing, no regex on prose.

Results are then:

1. combined via the `decision` rule,
2. written to `eval.jsonl` (one JSON line per mail, all outcomes, costs, latencies),
3. attached to the mail as `X-AIClassifier-Verdict`,
4. written to the quarantine (`spam_dir/*.eml` + `*.json`) when spam (or always, with
   `log_all_mails = true`),
5. counted into the dashboard stats.

The question and its acceptance criteria live in `[openrouter.question]` — that is the
main dial you tune.

**Default models (all 5):** `microsoft/microsoft-decision-1`, `cloudflare/clef-omni`,
`upstage/solar-decide-flash`, `nace-ai/drex-v1.5`, `typesafe/jev-1.13`.
Restrict via `[openrouter].models` or `OPENROUTER_MODELS`.

Endpoint contract: `prompt.md`. A legacy mode (`MODELS=name=url`) for custom
`{spam,score,reason}` endpoints still works as a fallback.

---

## Fail-open guarantee

This is the most important property of a mail filter. It must never be the reason
mail is lost.

* Every model call has its own timeout (`model_timeout_secs`). Model errors **never**
  count as spam.
* All models failing → `Continue` + `X-AIClassifier-Verdict: temperror` (mail is delivered).
* No panics on the hot path (no `unwrap` after startup).
* If the **process itself** dies, Postfix must still accept. That requires the MTA
  config — `milter_default_action = accept` globally **and** `default_action=accept`
  on the `smtpd_milters` entry (see [Postfix](#postfix)).

```sh
# verify the fail-open path works (stop the milter, mail still flows)
systemctl stop ai-classification-milter
sendmail -v someone@example.com < test.eml   # must be delivered
systemctl start ai-classification-milter
```

---

## Deployment

### systemd

`deploy/` contains a hardened unit plus an idempotent installer.

```sh
sudo deploy/install.sh --start          # or: just deploy
```

The installer creates the `_ai-milter` system user, installs the binary, seeds the
config **without overwriting existing files**, and prints the Postfix snippet.

<details>
<summary>The unit (deploy/ai-classification-milter.service)</summary>

```ini
[Unit]
Description=ai-classification-milter (Postfix pre-queue milter, OpenRouter Decisions API)
Documentation=https://github.com/rmoriz/ai-classification-milter
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=_ai-milter
Group=_ai-milter
EnvironmentFile=/etc/ai-classification-milter.env
ExecStart=/usr/local/sbin/ai-classification-milter
Restart=always
RestartSec=3
TimeoutStopSec=10
StandardOutput=journal
SyslogIdentifier=ai-classification-milter

StateDirectory=ai-classification-milter
StateDirectoryMode=0750
WorkingDirectory=/var/lib/ai-classification-milter

NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
RestrictRealtime=true
LockPersonality=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
CapabilityBoundingSet=
AmbientCapabilities=
UMask=0077
MemoryMax=256M

[Install]
WantedBy=multi-user.target
```

</details>

Two things worth understanding:

* **`StateDirectory=` instead of `ReadWritePaths=`.** A `ReadWritePaths=` entry that
  points at a non-existent directory makes systemd abort **before the process starts**
  (`status=226/NAMESPACE`, "Failed to set up mount namespacing"). `StateDirectory=`
  creates the directory with the right owner and keeps it writable under
  `ProtectSystem=strict` — no manual `mkdir`, no such failure mode.
* **Secrets and config** live in `/etc/ai-classification-milter.env` (`root:_ai-milter`,
  `0640`) and `/etc/ai-classification-milter.toml`. Keep `MILTER_CONFIG` pointing at
  the latter and leave the API key in the env file.

Default file locations:

| What | Path | Owner |
| --- | --- | --- |
| Binary | `/usr/local/sbin/ai-classification-milter` | root |
| Env / secret | `/etc/ai-classification-milter.env` | `root:_ai-milter` 0640 |
| Config | `/etc/ai-classification-milter.toml` | `root:_ai-milter` 0640 |
| Quarantine | `/var/lib/ai-classification-milter/spam-quarantine` | `_ai-milter` 0750 |
| Eval log | `/var/lib/ai-classification-milter/eval.jsonl` | `_ai-milter` 0640 |

### Postfix

```sh
postconf -e 'smtpd_milters = { inet:127.0.0.1:8891, connect_timeout=10s, content_timeout=30s, default_action=accept }'
postconf -e 'milter_protocol = 6'
postconf -e 'milter_default_action = accept'      # crash-accept, global
postfix reload
```

Keep `content_timeout` (30s) comfortably **above** `model_timeout_secs` (5s), so the
milter answers before the MTA gives up. Both `default_action=accept` settings are
required — without them a dead milter means queued mail.

Only `smtpd`-delivered mail is filtered: mail injected locally via `sendmail(1)` /
`pickup` does not pass through a milter (see [Troubleshooting](#troubleshooting)).

### Sendmail-only hosts

Nothing here is Postfix-specific — any milter-speaking MTA works (Sendmail, Exim).
Only the socket wiring differs; keep the equivalent "accept on timeout/crash"
behaviour.

---

## Configuration

`ai-classification-milter.toml` (path via `MILTER_CONFIG`).

**Precedence: env var > TOML > built-in default.** Every option has an env
equivalent — set one or the other, not both. Comments show the default value.

```toml
# ─── Milter socket ───────────────────────────────────────────────────────────
# Must match `smtpd_milters` in Postfix.
listen = "127.0.0.1:8891"

# ─── Models ──────────────────────────────────────────────────────────────────
[openrouter]
url  = "https://openrouter.ai/api/alpha/decisions"  # env: OPENROUTER_URL
# api_key = "sk-or-..."                              # env: OPENROUTER_API_KEY
models = [                       # env: OPENROUTER_MODELS (comma-separated)
  "microsoft/microsoft-decision-1",
  "cloudflare/clef-omni",
  "upstage/solar-decide-flash",
  "nace-ai/drex-v1.5",
  "typesafe/jev-1.13",
]
# referer = "https://example.com"    # HTTP-Referer, for OpenRouter rankings
# title   = "ai-classification-milter"  # X-Title

# ─── What the models are asked ───────────────────────────────────────────────
# The main dial you tune. One Noul question per mail; the answer is the
# yes-probability that `score_threshold` is applied to.
[openrouter.question]
# env: QUESTION_INSTRUCTIONS
instructions = "Is this mail spam, phishing or malicious?"
# env: QUESTION_TRUE
criteria_true = "The mail is unsolicited bulk mail, phishing, scam, fraud, or contains malicious content, links, or attachments."
# env: QUESTION_FALSE
criteria_false = "The mail is legitimate correspondence, an expected newsletter with List-Unsubscribe and aligned authentication, or an expected transactional notification."

# ─── Timing and decision ─────────────────────────────────────────────────────
[limits]
model_timeout_secs   = 5       # env: MODEL_TIMEOUT_SECS — per model, not total
score_threshold    = 0.85    # env: SCORE_THRESHOLD — score >= this counts as spam
decision           = "any"   # env: DECISION — any|majority|all (enforce mode only)

# Memory caps. RAM per mail ~= headers + body cap; nothing is unbounded.
header_max_count       = 200      # env: HEADER_MAX_COUNT
header_max_value_chars = 2048     # env: HEADER_MAX_VALUE_CHARS
body_max_bytes         = 100000   # env: BODY_MAX_BYTES — buffered, not sent
model_body_chars         = 4000     # env: MODEL_BODY_CHARS — excerpt sent to models

# ─── Quarantine, eval log, mode ──────────────────────────────────────────────
[logging]
spam_dir       = "./spam-quarantine"  # env: LOG_SPAM_DIR — *.eml + *.json, has PII
eval_log       = "./eval.jsonl"        # env: EVAL_LOG — one JSON line per mail
log_all_mails  = false                 # env: LOG_ALL_MAILS — true logs ham too
learn_mode     = true                  # env: LEARN_MODE — true = tag + log, never reject

# ─── Senders that skip the models entirely ──────────────────────────────────
# Matched on the envelope sender domain: case-insensitive, subdomains included
# (mail.example.com matches example.com), lookalikes never match. Such mail is
# accepted immediately with X-AIClassifier-Verdict: whitelisted.
[whitelist]
sender_domains = ["example.com", "dept.example.com"]   # env: WHITELIST_SENDER_DOMAINS

# ─── Optional web dashboard ──────────────────────────────────────────────────
# Off by default. Own socket, plain HTTP, no dependencies. See below.
[dashboard]
enabled  = false                 # env: DASHBOARD_ENABLED
listen   = "127.0.0.1:8892"      # env: DASHBOARD_LISTEN — bind a tailnet IP to share
# token            = "s3cret"   # env: DASHBOARD_TOKEN — required as ?token= or Bearer
# scramble_senders = false       # env: DASHBOARD_SCRAMBLE_SENDERS — blur for screenshots
```

**Modes.** `learn_mode = true` → never reject, only tag and log (stabilisation phase).
`learn_mode = false` → spam gets `550 5.7.1` (pre-queue, so no bounce/backscatter).
`decision` only takes effect in the second mode: `any` rejects as soon as one model
says spam, `majority` on a majority of the successful answers, `all` on unanimity.

**Whitelist.** CLI mode has no envelope, so it falls back to the `From:` header.

An annotated, copy-ready copy lives in `ai-classification-milter.toml.example`;
`deploy/ai-classification-milter.env.example` lists the env-file equivalents.

### Dashboard

Off by default; enable with `enabled = true` in the `[dashboard]` section above.

| Route | Purpose |
| --- | --- |
| `/` | Counters since start, per-model stats, last mails; auto-refreshes every 3s |
| `/mails` | Quarantine browser: search, detail view, raw download |
| `POST /mails/<stem>/release` | Re-deliver a false positive (needs `release = true`) |
| `/api/stats`, `/api/mails`, `/api/mails/<stem>` | The same data as JSON |

**Release (false positives).** With `release = true` each spam row in `/mails` gets a
*Deliver anyway* button. It hands the message back to the MTA via `sendmail`, which
enters through `pickup` — and since milters do not see `pickup`, the released mail is
**not filtered again**. That also means no allow-list entry is needed.

The mail is stamped with `X-AIClassifier-Released: <ts> by dashboard` and an audit
entry (`"event":"released"`) is appended to `eval.jsonl`.

Guardrails, because delivering a quarantined mail is irreversible:

* Off by default, and independent of `enabled` — it is its own switch.
* **POST only**, behind a `confirm()` in the browser. A link, a prefetch or a click
  in a screenshot cannot trigger it.
* Reuses the same token auth as everything else.
* The message goes to `sendmail` over stdin and every argument is a separate `exec`
  argument — no shell, so a crafted envelope sender cannot inject options or headers.
  Senders containing whitespace, CR/LF or without `@` are refused outright.
* Override the binary with `MILTER_SENDMAIL_CMD` (absolute path, no arguments) if it
  is not at `/usr/sbin/sendmail`.

Model results render as colored chips (`jev-1.13 · SPAM 0.96`) instead of a dense string,
in the UI and in the JSON (`models[]`).

The detail view shows the mail as **source** in a scroll container (never rendered as
HTML, so no remote content is loaded). Double-click selects the whole message; the
`copy exact` button fetches `/mails/<stem>/raw` and puts it on the clipboard.

> Use **copy exact** for pasting elsewhere. Copying straight from the page is fine for
> reading, but the browser normalises CRLF to LF in the rendered text, which subtly
> changes a mail. The `wrap` toggle is purely visual.

`scramble_senders` blurs addresses in HTML (CSS) and masks them as `•••@•••` in JSON —
screenshot hygiene, not access control. The `Scramble: on/off` link toggles it live via
`?scramble=1/0`, no restart needed.

### Migrating from the `LLM_*` names

The project talks to a **decision model** (the OpenRouter Decisions API / Noul), not a
chat LLM, so the `LLM_*` prefix was removed. The old names still work but log a
deprecation warning on startup.

| Old | New |
| --- | --- |
| `X-LLM-Verdict` header | `X-AIClassifier-Verdict` |
| `LLM_TIMEOUT_SECS` | `MODEL_TIMEOUT_SECS` |
| `LLM_BODY_CHARS` | `MODEL_BODY_CHARS` |
| `LLM_API_KEY` | `MODEL_API_KEY` (or `OPENROUTER_API_KEY`) |
| `[limits].llm_timeout_secs` | `[limits].model_timeout_secs` |
| `[limits].llm_body_chars` | `[limits].model_body_chars` |

**The header rename affects delivered mail and any Postfix rule that inspects it.**
Update those before you deploy:

```sh
# find rules depending on the old header
grep -rn 'X-LLM-Verdict' /etc/postfix/ /etc/*.cf 2>/dev/null
```

If you must roll back, older binaries still read the old env names, but only the new
binary writes `X-AIClassifier-Verdict`.

> **Plain HTTP is fine on a tailnet.** With Tailscale/NetBird, bind `listen` to the
> tailnet IP (`100.x.y.z:8892`) — WireGuard already encrypts the transport, so HTTPS is
> unnecessary. Keep `token` set anyway: a tailnet is a shared network.

---

## CLI mode

Evaluate a mail without Postfix:

```sh
ai-classification-milter -i < mail.eml            # table
ai-classification-milter -i --json < mail.eml     # JSON
ai-classification-milter -c /path/config.toml -i < mail.eml
```

Paste a mail and press Ctrl+D (header unfolding handled). All models are queried in
parallel; errors don't count as spam. Exit code is always 0 — it is an evaluation tool,
not a filter.

---

## Operations

**Logs and data**

```sh
journalctl -u ai-classification-milter -f          # runtime logs
systemctl status ai-classification-milter --no-pager
ss -tlnp | grep -E '8891|8892'                     # sockets actually bound
```

**Cost and accuracy** — `eval.jsonl` has one line per mail with per-model
`score`, `latency_ms`, `cost_usd`, `error`:

```sh
jq -s 'group_by(.outcomes[].model)
       | map({model: .[0].outcomes[0].model, n: length,
              spam: ([.[] | select(.spam)] | length),
              cost_usd: ([.[].outcomes[].cost_usd // 0] | add)})' eval.jsonl
```

**Rotation.** The quarantine is PII; keep a retention window.
`deploy/retention.cron.daily.example` deletes files older than 30 days — install it as
`/etc/cron.daily/ai-classification-milter`.

### Upgrading

```sh
git pull && just check && just deploy
```

The installer replaces the binary and unit, keeps config and data. Dashboard counters
reset on restart (they cover the current process lifetime); `eval.jsonl` persists.

### Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| `status=226/NAMESPACE`, "Failed to set up mount namespacing" | A `ReadWritePaths=` target does not exist. Use `StateDirectory=` or create the dir. |
| Service won't start, no log output | Missing `EnvironmentFile` or `MILTER_CONFIG` target → check `journalctl -u …`. |
| `401` on dashboard | Missing/ wrong `token`; send `Authorization: Bearer …`. |
| No `X-AIClassifier-Verdict` header on a mail | Mail came in via `pickup` (local `sendmail`) — milters only see `smtpd`. Or `smtpd_milters` doesn't match. Check the `smtpd` line in the mail log. |
| `X-AIClassifier-Verdict: temperror` | All models timed out / API unreachable → mail was accepted (fail-open). Check key, quota, `model_timeout_secs`. |
| Mail stuck in queue | `content_timeout` shorter than the milter's worst case, or crash-accept not configured. |
| Dashboard empty after restart | Stats are per-process. Historical data is in `eval.jsonl`. |

---

## Development

```sh
just build          # debug build
just check          # fmt --check + clippy -D warnings + tests   (pre-push gate)
just fmt            # format
just test           # tests only
just build-linux-amd64   # static musl binary (cross, via zig)
just cov                # coverage report (needs cargo-llvm-cov)
just build-macos-arm     # macOS/arm64 binary
just build-all           # both
just deploy              # build linux + install/start via deploy/install.sh
```

* Rust 2021, tokio + indymilter, no async runtime knobs to tune.
* Tests are inline (`cargo test`) and cover the decision rules, fail-open behaviour,
  config parsing and the dashboard request handling.
* Cross-compiling to Linux musl needs `just setup` (targets + `cargo-zigbuild`).
* CI (`.github/workflows/ci.yml`) runs tests and builds on every push to `main` and
  publishes rolling prerelease binaries on the `latest` tag.

### Repo layout

```
src/main.rs        milter callbacks, config, OpenRouter client, logging, CLI
src/dashboard.rs   optional web UI: stats, quarantine browser, live refresh
deploy/            systemd unit, env example, installer, retention job
prompt.md          system prompt / decision endpoint contract
justfile           build, check and deploy recipes
```

---

## Security & privacy

* **The quarantine contains PII** — full bodies, addresses, header chains. Protect the
  directory (0750, service user only), plan retention, and keep the dashboard on
  `127.0.0.1` or a tailnet IP with `token` set.
* **Scrambling is cosmetic.** CSS blur can be undone in devtools; it exists so
  screenshots don't leak addresses by accident, not as access control.
* **Mail content leaves the host.** Headers and a body excerpt are sent to the model
  API. Whitelist your trusted senders if that is a concern for their mail.
* The service runs unprivileged (`_ai-milter`), `NoNewPrivileges`, `ProtectSystem=strict`,
  empty capability set, `UMask=0077`.

---

## Notes

* Body is received from the start but capped (`body_max_bytes`); only `model_body_chars`
  go to the models. RAM per mail ≈ headers + body cap — no unbounded buffering.
* `decision` only applies in enforce mode (`learn_mode = false`).
* Spam log = PII: retention and rotation are a data-protection decision.