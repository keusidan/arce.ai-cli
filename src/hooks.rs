//! Hooks: the user's own commands at fixed moments.
//!
//! Events: `prompt` (a message is about to be sent; what the hook prints is added to it), `pre_tool`
//! (a tool is about to run; exit code 2 refuses it and its stderr is told to the model), `post_tool`
//! (a tool ran) and `stop` (a reply ended). The hook gets a JSON object on stdin. Hooks come only from
//! the user's settings or a trusted folder's.

use std::process::Stdio;
use std::time::Duration;

use globset::Glob;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::settings::Hook;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct HookResult {
    /// a hook refused (exit code 2), with what it said
    pub blocked: Option<String>,
    /// what the hooks printed on stdout (for `prompt`: added to the message)
    pub output: String,
    /// problems running hooks (shown, not fatal)
    pub errors: Vec<String>,
}

fn matches(hook: &Hook, event: &str, tool: &str) -> bool {
    if hook.event != event {
        return false;
    }
    if hook.matcher.trim().is_empty() || tool.is_empty() {
        return true;
    }
    Glob::new(hook.matcher.trim()).map(|g| g.compile_matcher().is_match(tool)).unwrap_or(false)
}

pub async fn run(hooks: &[Hook], event: &str, tool: &str, payload: &Value, cwd: &std::path::Path, timeout: Duration) -> HookResult {
    let mut result = HookResult::default();
    for hook in hooks.iter().filter(|h| matches(h, event, tool) && !h.command.trim().is_empty()) {
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.arg("/C").arg(&hook.command);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(&hook.command);
            c
        };
        cmd.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        cmd.env("ARCE_HOOK_EVENT", event).env("ARCE_HOOK_TOOL", tool);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                result.errors.push(format!("フック「{}」を始められません: {e}", hook.command));
                continue;
            }
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.to_string().as_bytes()).await;
        }
        let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Err(_) => {
                result.errors.push(format!("フック「{}」が {} 秒で終わりませんでした", hook.command, timeout.as_secs()));
                continue;
            }
            Ok(Err(e)) => {
                result.errors.push(format!("フック「{}」が失敗しました: {e}", hook.command));
                continue;
            }
            Ok(Ok(o)) => o,
        };
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        match out.status.code() {
            Some(0) => {
                if !stdout.is_empty() {
                    if !result.output.is_empty() {
                        result.output.push('\n');
                    }
                    result.output.push_str(&stdout);
                }
            }
            Some(2) => {
                result.blocked =
                    Some(if stderr.is_empty() { format!("フック「{}」が止めました", hook.command) } else { stderr });
                return result;
            }
            code => result.errors.push(format!(
                "フック「{}」が終了コード {} で終わりました{}",
                hook.command,
                code.map_or("-".into(), |c| c.to_string()),
                if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
            )),
        }
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn hook(event: &str, matcher: &str, command: &str) -> Hook {
        Hook { event: event.into(), matcher: matcher.into(), command: command.into() }
    }

    #[tokio::test]
    async fn exit_two_blocks_and_stdout_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = vec![
            hook("pre_tool", "run_*", "grep -q 'rm -rf' && { echo 'no rm' >&2; exit 2; } || exit 0"),
            hook("prompt", "", "echo extra context"),
        ];
        let t = Duration::from_secs(5);
        let r = run(&hooks, "pre_tool", "run_command", &json!({"input": {"command": "rm -rf /"}}), dir.path(), t).await;
        assert_eq!(r.blocked.as_deref(), Some("no rm"));
        let r = run(&hooks, "pre_tool", "run_command", &json!({"input": {"command": "ls"}}), dir.path(), t).await;
        assert_eq!(r, HookResult::default());
        let r = run(&hooks, "pre_tool", "read_file", &json!({"input": {"command": "rm -rf"}}), dir.path(), t).await;
        assert!(r.blocked.is_none());
        let r = run(&hooks, "prompt", "", &json!({}), dir.path(), t).await;
        assert_eq!(r.output, "extra context");
    }
}
