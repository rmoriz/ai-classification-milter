# ai-classification-milter

All headers as state -> N decision models in parallel (OpenRouter Decisions API, Noul) -> tag/reject + eval log.

## Fail-open guarantee (important)

* Every model call has its own timeout (`llm_timeout_secs`); errors never count
  as spam. Total outage of all models -> `Continue` + `X-LLM-Verdict: temperror`.
* No hot path panics (no `unwrap` after startup).
* Postfix must accept on crash/timeout — in `main.cf`: `milter_default_action = accept`
  globally AND `default_action=accept` on the `smtpd_milters` entry (see below).
  Postfix then accepts the mail even if the milter process is dead or too slow.

## Build & run

```sh
cd ai-classification-milter
cp ai-classification-milter.toml.example ai-classification-milter.toml  # adapt
export OPENROUTER_API_KEY=sk-or-...          # secret via env, not into TOML
cargo build --release
./target/release/ai-classification-milter
```

Config: `ai-classification-milter.toml` (path via `MILTER_CONFIG`), precedence env > TOML > defaults.
All 5 eval models are the default (`microsoft/microsoft-decision-1`,
`cloudflare/clef-omni`, `upstage/solar-decide-flash`, `nace-ai/drex-v1.5`,
`typesafe/jev-1.13`); restrict via `models` / `OPENROUTER_MODELS`.

`learn_mode = true` (default): never reject, only tag `X-LLM-Verdict` + log spam to
`spam_dir/*.eml + *.json`. For the stabilization phase.
`learn_mode = false`: spam -> `550 5.7.1` reject (pre-queue, no bounce/backscatter).

Whitelist sender domains (`[whitelist] sender_domains`, env
`WHITELIST_SENDER_DOMAINS=example.com,dept.example.com`): matching mail never hits the
models and is auto-accepted with `X-LLM-Verdict: whitelisted`. Match is on the
envelope sender domain (case-insensitive, subdomains included, e.g.
`mail.example.com` matches `example.com`). CLI mode (no envelope) falls back to the
`From:` header.

## CLI mode (without Postfix)

```sh
./target/release/ai-classification-milter -i < mail.eml          # table
./target/release/ai-classification-milter -i --json < mail.eml   # JSON
./target/release/ai-classification-milter -c /path/config.toml -i < mail.eml
```

Paste a mail + Ctrl+D (header unfolding included). All models are queried in
parallel, errors don't count as spam. Exit code always 0 (eval tool, not a filter).

## Postfix (`main.cf`) — with crash-accept

```
smtpd_milters = { inet:127.0.0.1:8891, connect_timeout=10s, content_timeout=30s, default_action=accept }
milter_protocol = 6
milter_default_action = accept
```

Keep `milter_content_timeout` (30s) > `llm_timeout_secs` (5s).
Reload: `postfix reload`. Test: `tail -f /var/log/mail.log`, send a mail,
check `spam-quarantine/` + `X-LLM-Verdict` header.

## Multi-model eval (Decisions API)

```
# ai-classification-milter.toml [openrouter].models (default = all 5):
microsoft/microsoft-decision-1, cloudflare/clef-omni, upstage/solar-decide-flash,
nace-ai/drex-v1.5, typesafe/jev-1.13
```

* One endpoint (`POST {url}` = `https://openrouter.ai/api/alpha/decisions`),
  one request per model per mail — in parallel (`join_all`), each with its own
  `llm_timeout_secs`. A slow model never blocks the others past the timeout.
  Docs: https://openrouter.ai/docs/guides/community/jev-tutorial
* `state` = envelope + **all headers** (as an array under `email-headers`,
  duplicates like `Received` preserved) + body excerpt. One `noul` question
  `is_spam` ("Is this mail spam, phishing or malicious?"), answer = yes-probability
  0–1 — no free-text parsing. Question/criteria maintained in `[openrouter.question]`.
* `eval.jsonl`: one line per mail with all outcomes
  (`model, spam, score (=noul), latency_ms, cost_usd, error`) — directly comparable,
  e.g. `jq -s 'group_by(.outcomes[].model)'`, including per-model cost (`usage.cost`).
* `X-LLM-Verdict` header summarizes all models compactly
  (`a=spam.92 | b=ham.10 | c=err:...`), max 500 chars.
* `decision` only matters for enforce mode (`learn_mode = false`): `any`
  rejects as soon as >= 1 model says spam, `majority` on majority of the
  successful answers, `all` only on unanimity.

## Decision endpoint contract (OpenRouter Decisions API, identical per model)

See `prompt.md`. Legacy mode (`MODELS=name=url`) for custom
`{spam,score,reason}` endpoints remains as a fallback.

## Dashboard (optional)

```toml
[dashboard]
enabled = true
listen = "127.0.0.1:8892"   # interface + port, own socket next to the milter port
# token = "s3cret"           # optional: then required as ?token= or Authorization: Bearer
```

Or via env: `DASHBOARD_ENABLED=true`, `DASHBOARD_LISTEN=127.0.0.1:8892`,
`DASHBOARD_TOKEN=s3cret`. No new dependency — tiny built-in HTTP server
(plain HTTP, no TLS needed on trusted networks).

* `/` — counters since process start (total/spam/ham/whitelisted/temperror),
  per-model stats (ok/spam/err, avg score/latency, cost) + last 20 mails.
  Auto-refreshes every 3s via `fetch` polling of `/api/stats` (no WebSocket/SSE).
* `/mails?q=&limit=&offset=` — browse the quarantine (`spam_dir`), newest first,
  search by sender/filename, detail view with meta + `.eml` preview + raw download.
* `/api/stats`, `/api/mails`, `/api/mails/<stem>` — same as JSON.

Caution: the quarantine contains PII — keep it on `127.0.0.1` or behind a reverse
proxy with auth; set `token` whenever the port isn't local-only. For Tailscale/Netbird:
bind `listen` to the tailnet IP (e.g. `100.x.y.z:8892`) — HTTPS is unnecessary there
(WireGuard already encrypts), but still set `token` (shared network).

## Notes

* Body is received from the start (`on_body`), but capped (`body_max_bytes`,
  default 100KB). Only `llm_body_chars` (default 4000) go to the models as excerpt.
* RAM per mail ~ headers (200 x 2048) + body cap. No unbounded buffering.
* Spam log contains PII: protect the directory, plan retention + rotation
  (e.g. `tmpwatch`, 30 days); ham is only logged with `log_all_mails = true`.
* Fail-open: model error/timeout -> `Continue` + `X-LLM-Verdict: temperror`.
