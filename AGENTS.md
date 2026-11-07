# arce.ai-cli — notes for coding agents

ARCE.AI in the terminal: one Rust binary, `arce`. Design and the app's own logic come from the Flutter app `keusidan/arce.ai`; the feature set was shaped by a survey of ten public AI CLIs (`docs/cli_survey_*.md`, anonymized).

**Read first:** `docs/README.md`, the index of the documentation (one file per topic, Japanese). The docs describe the current specification only; no history.

## Commands

```bash
cargo build
cargo test                    # unit tests + tests/e2e.rs (the binary against a fake OpenAI-compatible server)
cargo clippy --all-targets    # keep it at zero warnings
cargo fmt                     # rustfmt.toml: max_width 140
```

## The user's standing instructions

- **Work on the `dev` branch.** The session branch the harness names (`claude/…`) gets the same commits. Do not create or delete other branches unless asked. No pull request unless asked.
- **Never write where the referenced CLI tools came from.** Their names, vendors, URLs and any other citation must not appear anywhere in the repository (code, comments, docs, commit messages, test names), because of copyright concerns. The survey calls them "Tool A" to "Tool J". Do not copy code, text or design from them; write everything ourselves. In chat replies to the user, sources may be given as usual.
- **Design and the app's own processing come from `keusidan/arce.ai`.** The colour tokens in `src/theme.rs` must stay equal to `lib/ui/theme.dart` of the app. When porting more logic, port it faithfully (wording, defaults, edge cases, tests) and record it in `docs/arce_heritage_*.md`.
- **Anything an agent needs, and every additional instruction from the user, goes into this file, in English.**
- Language: Rust (optimized, idiomatic) unless the user names another; avoid Python wherever something else will do (also for scripts and tooling).
- Reply to the user in Japanese, times in JST. Write an abbreviation as `略称(正式名称)` the first time. Keep English technical terms (boxing, interface …) in English. Give sources for explanations in chat.
- A new docs file gets the suffix `Claude-MM-dd-yyyy_HH-mm-ss` (JST) before its extension, e.g. `docs/spec_Claude-11-08-2026_08-07-02.md`. Exceptions (no suffix, name unchanged): files a tool or convention reads by name (`AGENTS.md`, `README.md`, `Cargo.toml`, `rustfmt.toml`, `.gitignore`, workflow files), Rust sources (the file name is the module name), and edits of existing files. When unsure, ask.
- Docs are the current specification only: when behaviour changes, edit the topic file that owns it, in place. Write intent, decisions, constraints and what is unverified; not what the code already says.
- Commit trailers: the `Co-Authored-By` and `Claude-Session` lines the session gives.

## Rules that are easy to break

- **Everything except secrets is a setting** (`src/settings.rs`). A new limit, timeout or count gets a field with a default, not a constant. Keys and tokens live only in `secrets.json` (0600) or environment variables — never in `settings.toml`, a session file, an approval card, an error message or a tool result.
- **Chat cannot change where things are sent.** `update_settings` only accepts keys in `CHAT_CHANGEABLE`, always asks, and `blocked_reason` explains the rest (providers, fallback, permissions, mode, hooks, MCP, secrets, developer models, search).
- **Untrusted project settings are not read for destinations or commands** (`UNTRUSTED_BLOCKED`). Do not widen what an untrusted `.arce/settings.toml` may set.
- **Permissions:** deny always wins; `update_settings` always asks; commands are never remembered by "always" (`approved`); an allow rule covers a shell line only when it covers every part, and never a line with `$(…)`, backticks, redirections or open quotes (`split_shell`); commands after external content ask (`confirm_after_external`); commands with secrets ask. Keep the tests in `src/permissions.rs` passing and add one for every new rule.
- **Secrets:** placeholders are substituted only in `http_request` (path, query, header values, body — never scheme or host) and `run_command`; the final URL must pass `secrets::hosts::problem` for every secret used; every tool result goes through `Book::redact`. Each rule in `secrets.rs` has a test written the way an attacker would write the address — keep them.
- **The system prompt has no clock and no counters** (`src/prompt.rs`), so providers can cache the prefix. A test checks this.
- **Every tool call gets a result** in the history (also when refused or stopped); a dangling call is dropped when a session is opened (`without_dangling_calls`).
- **Fallback only before the first output** (`ProviderError::before_output`); after text has been shown, a failure is shown, not hidden by another model.
- **A failed tool call is not a failed reply:** tell the model why (`Error: …`) and let it write the call again; only repeats (`repeat_limit`), the round limit and `retry_limit` end a reply.
- Colours only when stdout is a terminal and `NO_COLOR` is not set; widths are display widths (East Asian wide = 2).
- Tests must not assume `/` as the path separator in assertions that compare paths, and unix-only tests (`sh`) are `#[cfg(unix)]`.
- Never delete a folder by following symbolic links; `/undo` restores and removes only the files listed in its manifest.
