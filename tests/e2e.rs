//! The binary against a fake OpenAI-compatible server: tools, permissions, the text tool form and
//! fallback, end to end.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One scripted answer: an HTTP status and the SSE chunks (`data:` payloads).
#[derive(Clone)]
struct Reply {
    status: u16,
    chunks: Vec<Value>,
}

fn sse(chunks: Vec<Value>) -> Reply {
    Reply { status: 200, chunks }
}

fn text_reply(t: &str) -> Reply {
    sse(vec![
        json!({"choices":[{"delta":{"content": t}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3}}),
    ])
}

fn tool_reply(name: &str, args: Value) -> Reply {
    sse(vec![
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":name,"arguments":""}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":args.to_string()}}]}}]}),
        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
    ])
}

struct Server {
    url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
}

/// Serves [replies] in order (the last one again when they run out).
async fn serve(replies: Vec<Reply>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(vec![]));
    let seen = bodies.clone();
    tokio::spawn(async move {
        let mut n = 0usize;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let reply = replies[n.min(replies.len() - 1)].clone();
            n += 1;
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = vec![];
                let mut tmp = [0u8; 8192];
                let body_start;
                loop {
                    let k = sock.read(&mut tmp).await.unwrap();
                    buf.extend_from_slice(&tmp[..k]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        body_start = p + 4;
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                let len: usize =
                    head.lines().find_map(|l| l.strip_prefix("content-length:")).map(|v| v.trim().parse().unwrap()).unwrap_or(0);
                while buf.len() < body_start + len {
                    let k = sock.read(&mut tmp).await.unwrap();
                    buf.extend_from_slice(&tmp[..k]);
                }
                let body: Value = serde_json::from_slice(&buf[body_start..body_start + len]).unwrap_or(Value::Null);
                seen.lock().unwrap().push(body);
                let mut out = if reply.status == 200 {
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n".to_string()
                } else {
                    format!(
                        "HTTP/1.1 {} Error\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{{\"error\":{{\"message\":\"rate limited\"}}}}",
                        reply.status
                    )
                };
                if reply.status == 200 {
                    for c in &reply.chunks {
                        out.push_str(&format!("data: {c}\n\n"));
                    }
                    out.push_str("data: [DONE]\n\n");
                }
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    Server { url, bodies }
}

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new(settings: &str) -> Home {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home/config")).unwrap();
        std::fs::create_dir_all(dir.path().join("work")).unwrap();
        std::fs::write(dir.path().join("home/config/settings.toml"), settings).unwrap();
        Home { dir }
    }
    fn work(&self) -> std::path::PathBuf {
        self.dir.path().join("work")
    }
    async fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_arce"))
            .args(args)
            .current_dir(self.work())
            .env("ARCE_HOME", self.dir.path().join("home"))
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

fn settings_for(url: &str, extra: &str) -> String {
    format!(
        "model = \"fake|m1\"\n{extra}\n[[providers]]\nid = \"fake\"\nbase_url = \"{url}\"\n[[providers.models]]\nid = \"m1\"\ncontext_tokens = 100000\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_writes_a_file_and_the_reply_ends() {
    let server = serve(vec![tool_reply("write_file", json!({"path": "hello.txt", "content": "hi\n"})), text_reply("書きました。")]).await;
    let home = Home::new(&settings_for(&server.url, ""));
    let (code, stdout, stderr) = home.run(&["-p", "hello.txt を作って", "--mode", "auto-edit", "--output-format", "json"]).await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(std::fs::read_to_string(home.work().join("hello.txt")).unwrap(), "hi\n");
    let result: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(result["status"], "done");
    assert_eq!(result["result"], "書きました。");
    assert_eq!(result["tools"], 1);
    assert_eq!(result["usage"]["input_tokens"], 10);
    let bodies = server.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2);
    // the second request carries the call and its result with their roles
    let msgs = bodies[1]["messages"].as_array().unwrap();
    assert_eq!(msgs[msgs.len() - 2]["tool_calls"][0]["function"]["name"], "write_file");
    assert_eq!(msgs[msgs.len() - 1]["role"], "tool");
    assert!(bodies[0]["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "edit_file"));
    // nothing in the prompt changes between the two requests
    assert_eq!(bodies[0]["messages"][0], bodies[1]["messages"][0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_person_a_write_is_refused_and_the_model_is_told() {
    let server = serve(vec![tool_reply("write_file", json!({"path": "x.txt", "content": "x"})), text_reply("やめました。")]).await;
    let home = Home::new(&settings_for(&server.url, ""));
    let (code, _stdout, _stderr) = home.run(&["-p", "x.txt を作って"]).await;
    assert_eq!(code, 0);
    assert!(!home.work().join("x.txt").exists());
    let bodies = server.bodies.lock().unwrap().clone();
    let last = bodies[1]["messages"].as_array().unwrap().last().unwrap().clone();
    assert!(last["content"].as_str().unwrap().contains("非対話モードでは承認できません"), "{last}");
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_offers_no_writing_tools() {
    let server = serve(vec![text_reply("計画です。")]).await;
    let home = Home::new(&settings_for(&server.url, ""));
    let (code, stdout, _) = home.run(&["-p", "直して", "--mode", "plan"]).await;
    assert_eq!(code, 0);
    assert!(stdout.contains("計画です。"));
    let body = server.bodies.lock().unwrap()[0].clone();
    let names: Vec<&str> = body["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert!(!names.contains(&"write_file") && !names.contains(&"edit_file"));
    assert!(names.contains(&"read_file"));
    assert!(body["messages"][0]["content"].as_str().unwrap().contains("# Plan mode"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_text_form_reads_tags_and_sends_results_back_as_text() {
    let call = json!({"name": "list_dir", "arguments": {}}).to_string();
    let server = serve(vec![
        sse(vec![
            json!({"choices":[{"delta":{"content": format!("<think>見る</think>見ます<tool_call>{call}")}}]}),
            json!({"choices":[{"delta":{"content":"</tool_call>"}}]}),
        ]),
        text_reply("空です。"),
    ])
    .await;
    let settings = format!("model = \"fake|m1\"\n[[providers]]\nid = \"fake\"\nbase_url = \"{}\"\ntool_mode = \"text\"\n", server.url);
    let home = Home::new(&settings);
    let (code, stdout, stderr) = home.run(&["-p", "ファイルの一覧"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("空です。"));
    let bodies = server.bodies.lock().unwrap().clone();
    assert!(bodies[0].get("tools").is_none());
    assert!(bodies[0]["messages"][0]["content"].as_str().unwrap().contains("<tool_call>"));
    let msgs = bodies[1]["messages"].as_array().unwrap();
    assert!(msgs.last().unwrap()["content"].as_str().unwrap().contains("<tool_response>"));
    assert!(msgs.iter().all(|m| m["role"] != "tool"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rate_limited_model_falls_back_before_the_first_word() {
    let bad = serve(vec![Reply { status: 429, chunks: vec![] }]).await;
    let good = serve(vec![text_reply("代わりに答えます。")]).await;
    let settings = format!(
        "model = \"a|m\"\n[[fallback]]\nfrom = \"a|*\"\ntargets = [\"b|m\"]\n[[providers]]\nid = \"a\"\nbase_url = \"{}\"\n[[providers]]\nid = \"b\"\nbase_url = \"{}\"\n",
        bad.url, good.url
    );
    let home = Home::new(&settings);
    let (code, stdout, stderr) = home.run(&["-p", "hi"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("代わりに答えます。"));
    assert!(stderr.contains("a|m が HTTP 429 のため b|m に切り替えました"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_call_over_and_over_stops_the_reply() {
    let server = serve(vec![tool_reply("list_dir", json!({}))]).await;
    let home = Home::new(&settings_for(&server.url, ""));
    let (code, _stdout, stderr) = home.run(&["-p", "loop"]).await;
    assert_eq!(code, 1);
    assert!(stderr.contains("同じ道具を同じ引数で 3 回続けて呼んだので止めました"), "{stderr}");
    assert_eq!(server.bodies.lock().unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_continues_with_its_history() {
    let server = serve(vec![text_reply("一つ目"), text_reply("二つ目")]).await;
    let home = Home::new(&settings_for(&server.url, ""));
    assert_eq!(home.run(&["-p", "first"]).await.0, 0);
    assert_eq!(home.run(&["-p", "second", "-c"]).await.0, 0);
    let bodies = server.bodies.lock().unwrap().clone();
    let msgs = bodies[1]["messages"].as_array().unwrap();
    let texts: Vec<&str> = msgs.iter().skip(1).map(|m| m["content"].as_str().unwrap_or("")).collect();
    assert_eq!(texts, vec!["first", "一つ目", "second"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn untrusted_project_settings_cannot_redirect_the_model() {
    let server = serve(vec![text_reply("ok")]).await;
    let home = Home::new(&settings_for(&server.url, ""));
    std::fs::create_dir_all(home.work().join(".arce")).unwrap();
    std::fs::write(
        home.work().join(".arce/settings.toml"),
        "model = \"evil|x\"\n[[providers]]\nid = \"evil\"\nbase_url = \"https://evil.test/v1\"\n",
    )
    .unwrap();
    let (code, _stdout, stderr) = home.run(&["-p", "hi"]).await;
    // the model names a provider that was refused, so nothing is sent anywhere
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("信頼されていない"), "{stderr}");
    assert!(stderr.contains("プロバイダ evil がありません"), "{stderr}");
    assert!(server.bodies.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn coding_mode_splits_the_work_and_writes_the_developers_files() {
    let plan = json!({
        "plan": "flowchart TD\n  a[\"Model\"] --> b[\"View\"]",
        "summary": "Two files.",
        "tasks": [
            {"id": "a", "title": "Model", "instructions": "write model.txt", "files": ["model.txt"]},
            {"id": "b", "title": "View", "instructions": "write view.txt using the model", "files": ["view.txt"]}
        ]
    });
    let planner = serve(vec![tool_reply("delegate", plan), text_reply("できました。")]).await;
    let dev = serve(vec![text_reply("File: model.txt\n```text\nMODEL\n```\n"), text_reply("File: view.txt\n```text\nVIEW\n```\n")]).await;
    let settings = format!(
        "model = \"p|planner\"\n[coding]\nenabled = true\ndevelopers = [\"d|dev\"]\n[[providers]]\nid = \"p\"\nbase_url = \"{}\"\n[[providers]]\nid = \"d\"\nbase_url = \"{}\"\n",
        planner.url, dev.url
    );
    let home = Home::new(&settings);
    let (code, stdout, stderr) = home.run(&["-p", "アプリを実装して", "--mode", "auto-edit"]).await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(std::fs::read_to_string(home.work().join("model.txt")).unwrap(), "MODEL\n");
    assert_eq!(std::fs::read_to_string(home.work().join("view.txt")).unwrap(), "VIEW\n");
    // the job that waited was given the result it waited for
    let dev_bodies = dev.bodies.lock().unwrap().clone();
    assert_eq!(dev_bodies.len(), 2);
    let second = dev_bodies[1]["messages"][1]["content"].as_str().unwrap();
    assert!(second.contains("# Result of the job you waited for: d1 — Model"), "{second}");
    // the planner read the numbered results
    let report = planner.bodies.lock().unwrap()[1]["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
    assert!(report.contains("## d1 — Model") && report.contains("Files written: model.txt"), "{report}");
    assert!(planner.bodies.lock().unwrap()[0]["messages"][0]["content"].as_str().unwrap().contains("# Coding mode"));
}
