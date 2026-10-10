# AGENTS.md

Working notes for AI agents (and humans) changing this repo. Read before editing.

## What this is

A Postfix **pre-queue milter** in Rust that asks N decision models in parallel whether
a mail is spam/phishing, then tags or rejects it. Single binary, two modules, ~3.1k LOC.

- `src/main.rs` — milter callbacks, config loading, OpenRouter client, logging, CLI mode
- `src/dashboard.rs` — optional built-in web UI (stats + quarantine browser)
- `deploy/` — systemd unit, installer, env example, retention job

## Commands

```sh
just check      # fmt --check + clippy -D warnings + test  (the pre-push gate)
just fmt        # cargo fmt --all
just test       # cargo test
just deploy     # cross-build linux + install/start as systemd service
just cov        # coverage report (cargo llvm-cov)
```

Coverage (informational, ~76% lines; CI uploads it as an artifact, it does not
gate merges): `dashboard.rs` ~92%, `main.rs` ~61%. The uncovered remainder is
mostly the process entry point (`main`, milter socket setup, signal handling) and
the live HTTP calls to OpenRouter, which need a real API. Those paths need a
integration test with a mock server, not a unit test.

CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests on every push, then publishes
rolling prerelease binaries on the `latest` tag. Keep `just check` green — clippy runs
with `-D warnings`, so a new warning breaks CI.

## Invariants — do not break these

**1. Fail-open is the product.** This filter sits in front of mail delivery. If it is
wrong in the "block" direction, mail is lost.

- Model errors, timeouts and malformed answers must **never** count as spam.
- `combine()` returns `false` when there are zero successful answers.
- Total model failure → `Continue` + `X-LLM-Verdict: temperror`, never `Reject`.
- No `unwrap()` / `expect()` / `panic!` on the EOM hot path. Everything there returns a
  `Status` and lets the mail through. (Tests may `unwrap` freely.)
- The only `Status::Reject` in the codebase is the explicit `learn_mode = false` spam
  path. If you add another, justify it — that's the one line that can drop mail.

**2. Config precedence is env > TOML > default.** Every option follows
`env("X").or(toml_x).unwrap_or(default)`. New options must use the same shape and get a
row in `.env.example` and the README table.

**3. Secrets stay out of the TOML and the repo.** `OPENROUTER_API_KEY` via env;
`deploy/ai-classification-milter.env` is `root:_ai-milter` `0640`.

**4. Bounded memory.** Per mail: headers capped (`header_max_count` ×
`header_max_value_chars`) and body capped (`body_max_bytes`). Never buffer unbounded.

## Conventions

- No new dependencies without discussion. The dashboard is deliberately hand-rolled on
  `tokio::net::TcpListener` to keep the milter dependency-light; a previous attempt to
  add `socket2` for `SO_BINDTODEVICE` was reverted by request.
- `#![allow(dead_code)]` marks the legacy fallback paths (custom `MODELS=name=url`
  endpoints, free-text verdict parser, old config name `llm-milter.toml`). They are kept
  intentionally — don't delete them, and don't "clean them up" without asking.
- Tests are inline `#[cfg(test)] mod tests` at the bottom of each module (27 in `main`,
  26 in `dashboard`). Add new behaviour with a test next to it.
- Tests must not share filesystem state. Use `unique_tmp(tag)` in `main`'s test module
  for temp paths — a fixed `/tmp/eval.jsonl` made parallel runs flake. `#[tokio::test]`
  runs on reused runtime worker threads, so thread ids collide; the atomic counter in
  `unique_tmp` is what makes the path unique.
- Commit messages: `Area: what changed` as the subject (e.g. `Dashboard: stats +
  quarantine browser…`), then a bullet list of what and why.

## Dashboard specifics

- Plain HTTP, no TLS — intended for `127.0.0.1` or a tailnet IP (Tailscale/NetBird).
- Routes: `/`, `/mails`, `/mails/<stem>`, `/mails/<stem>/raw`, `/api/stats`,
  `/api/mails`, `/api/mails/<stem>`. GET and HEAD only.
- Auth: `Authorization: Bearer <token>` or `?token=`. Keep `token` and `scramble`
  propagated across generated links (`href_q`) and forms (`hidden_keep`) — dropping
  them logs you out mid-navigation.
- `?scramble=1|0` overrides the `scramble_senders` config per request, for screenshots.
  It is CSS blur in HTML + `•••@•••` in JSON — cosmetic, not access control. Don't
  describe it as security.
- All interpolated output goes through `html_escape`; mail stems go through `url_encode`.
  Keep it that way.
- Model results render as colored chips (`ModelChip`), not the pipe-joined string.
  If you change `ModelOutcome`'s serde shape, the dashboard's sidecar parser breaks —
  `eval.jsonl` and the `.json` sidecars are effectively a public data format.

## Gotchas that cost time before

- **systemd `226/NAMESPACE`** = a `ReadWritePaths=` target doesn't exist. The unit uses
  `StateDirectory=` so systemd creates the dir with the right owner. Don't reintroduce
  `ReadWritePaths=` for paths that may be absent.
- **Milters only see `smtpd`-delivered mail.** Local `sendmail`/`pickup` bypasses the
  milter entirely — a whitelist miss there is not a bug. Check the `pickup` vs `smtpd`
  line in the mail log.
- **Dashboard stats are per-process** (reset on restart); `eval.jsonl` is the history.
- Cross-building Linux musl needs `just setup` (targets + `cargo-zigbuild`). A
  Linux-only code path won't be caught by a macOS `cargo build` — run
  `just build-linux-amd64` when touching `#[cfg(target_os)]` code.

## Style

Comments explain *why*, not *what*. Keep them short. User-facing strings in the web UI
are English (the UI was fully translated; German strings crept back in once — check with
`grep -nE '[äöüß]' src/dashboard.rs` before finishing).