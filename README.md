# ai-classification-milter

Postfix pre-queue milter that asks **N decision models in parallel** whether a mail
is spam/phishing, then **tags or rejects** it — and never breaks mail delivery.

```
MAIL → RCPT* → HEADER* → BODY* → EOM
                              └→ N models in parallel (OpenRouter Decisions API / Noul)
                                   └→ X-LLM-Verdict header + eval.jsonl + quarantine
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
$EDITOR ai-classification-milter.toml
export OPENROUTER_API_KEY=sk-or-...     # secret via env, never into the TOML
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
grep -i X-LLM-Verdict /var/log/mail.log     # or check the delivered message headers
```

`X-LLM-Verdict: jev-1.13=spam.96 | clef-omni=spam.90 | ...`

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
| Body | excerpt, first `llm_body_chars` (default 4000) |

Each model answers **one Noul question** — `is_spam`: *"Is this mail spam, phishing or
malicious?"* — as a yes-probability `0.0–1.0`. No free-text parsing, no regex on prose.

Results are then:

1. combined via the `decision` rule,
2. written to `eval.jsonl` (one JSON line per mail, all outcomes, costs, latencies),
3. attached to the mail as `X-LLM-Verdict`,
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

* Every model call has its own timeout (`llm_timeout_secs`). Model errors **never**
  count as spam.
* All models failing → `Continue` + `X-LLM-Verdict: temperror` (mail is delivered).
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

Keep `content_timeout` (30s) comfortably **above** `llm_timeout_secs` (5s), so the
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

`ai-classification-milter.toml` (path via `MILTER_CONFIG`). **Precedence: env > TOML >
defaults.** Only set what differs.

| Key | Env | Default | Meaning |
| --- | --- | --- | --- |
| `listen` | `MILTER_LISTEN` | `127.0.0.1:8891` | Milter socket |
| `[openrouter].url` | `OPENROUTER_URL` | Decisions API | Endpoint |
| `[openrouter].api_key` | `OPENROUTER_API_KEY` | – | Prefer env |
| `[openrouter].models` | `OPENROUTER_MODELS` | all 5 | Comma-separated model IDs |
| `[openrouter].referer` / `.title` | `OPENROUTER_REFERER` / `_TITLE` | – | OpenRouter ranking metadata |
| `[openrouter.question].instructions` | `QUESTION_INSTRUCTIONS` | see example | The question asked |
| `…criteria_true` / `…criteria_false` | `QUESTION_TRUE` / `QUESTION_FALSE` | see example | What counts as spam vs. legitimate |
| `[limits].llm_timeout_secs` | `LLM_TIMEOUT_SECS` | `5` | Per-model timeout |
| `[limits].score_threshold` | `SCORE_THRESHOLD` | `0.85` | Score ≥ threshold counts as spam |
| `[limits].decision` | `DECISION` | `any` | `any` / `majority` / `all` (enforce mode) |
| `[limits].header_max_count` | `HEADER_MAX_COUNT` | `200` | Header cap |
| `[limits].header_max_value_chars` | `HEADER_MAX_VALUE_CHARS` | `2048` | Per-header value cap |
| `[limits].body_max_bytes` | `BODY_MAX_BYTES` | `100000` | Body buffer cap |
| `[limits].llm_body_chars` | `LLM_BODY_CHARS` | `4000` | Body excerpt sent to models |
| `[logging].spam_dir` | `LOG_SPAM_DIR` | `./spam-quarantine` | Quarantine directory |
| `[logging].eval_log` | `EVAL_LOG` | `./eval.jsonl` | Per-mail JSON lines |
| `[logging].log_all_mails` | `LOG_ALL_MAILS` | `false` | Log ham too (eval) |
| `[logging].learn_mode` | `LEARN_MODE` | `true` | `true` = tag + log, never reject |
| `[whitelist].sender_domains` | `WHITELIST_SENDER_DOMAINS` | `[]` | Domains that skip the models |
| `[dashboard].*` | `DASHBOARD_*` | see below | Optional web UI |

**Modes.** `learn_mode = true` → never reject, only tag and log (stabilisation phase).
`learn_mode = false` → spam gets `550 5.7.1` (pre-queue, so no bounce/backscatter).

**Whitelist.** Matching sender domains never reach the models and are accepted with
`X-LLM-Verdict: whitelisted`. Matching is on the envelope sender domain,
case-insensitive, subdomains included (`mail.example.com` matches `example.com`);
lookalikes (`notexample.com`, `example.com.attacker.com`) deliberately do not match.
CLI mode without an envelope falls back to the `From:` header.

### Dashboard

Optional, off by default, no extra dependencies (small built-in HTTP server).

```toml
[dashboard]
enabled = true
listen = "127.0.0.1:8892"     # own socket, independent from `listen`
token = "s3cret"              # required as ?token= or `Authorization: Bearer`
scramble_senders = false      # blur senders for shareable screenshots
```

| Route | Purpose |
| --- | --- |
| `/` | Counters since start, per-model stats, last mails; auto-refreshes every 3s |
| `/mails` | Quarantine browser: search, detail view, raw download |
| `/api/stats`, `/api/mails`, `/api/mails/<stem>` | The same data as JSON |

Model results render as colored chips (`jev-1.13 · SPAM 0.96`) instead of a dense string,
in the UI and in the JSON (`models[]`).

`scramble_senders` blurs addresses in HTML (CSS) and masks them as `•••@•••` in JSON —
screenshot hygiene, not access control. The `Scramble: on/off` link toggles it live via
`?scramble=1/0`, no restart needed.

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
| No `X-LLM-Verdict` header on a mail | Mail came in via `pickup` (local `sendmail`) — milters only see `smtpd`. Or `smtpd_milters` doesn't match. Check the `smtpd` line in the mail log. |
| `X-LLM-Verdict: temperror` | All models timed out / API unreachable → mail was accepted (fail-open). Check key, quota, `llm_timeout_secs`. |
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

* Body is received from the start but capped (`body_max_bytes`); only `llm_body_chars`
  go to the models. RAM per mail ≈ headers + body cap — no unbounded buffering.
* `decision` only applies in enforce mode (`learn_mode = false`).
* Spam log = PII: retention and rotation are a data-protection decision.