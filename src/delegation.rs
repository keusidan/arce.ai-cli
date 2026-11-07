//! The coding mode's `delegate`: the planner's jobs carried out by the developer models.
//!
//! The CLI only carries the jobs out: the split, whether a result is good, and whether to send it back
//! are the planner's. Jobs with nothing to wait for start at once on the least busy developer model;
//! a job that waits gets the results it waited for; a failed job's dependants are not run; a model
//! that fails hands its job to one it has not tried; a developer that asks a question is told once to
//! decide itself.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{Map, Value};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::llm::{self, ChatRequest, Message};
use crate::plan_graph::PlanGraph;
use crate::prompt;
use crate::provider::ProviderConfig;

#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub instructions: String,
    pub depends_on: Vec<String>,
    pub files: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DelegateArgs {
    pub plan: String,
    pub summary: String,
    pub tasks: Vec<Task>,
    pub revise: Option<(String, String)>,
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Reads and checks a `delegate` call; Err says what to write differently.
pub fn parse(a: &Map<String, Value>, max_tasks: usize) -> Result<DelegateArgs, String> {
    let revise = match a.get("revise") {
        Some(Value::Object(r)) => {
            let id = r.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let fb = r.get("feedback").and_then(Value::as_str).unwrap_or("").trim().to_string();
            if id.is_empty() || fb.is_empty() {
                return Err("revise には id（d1 など）と feedback が要ります".into());
            }
            Some((id, fb))
        }
        _ => None,
    };
    let plan = a.get("plan").and_then(Value::as_str).unwrap_or("").to_string();
    let summary = a.get("summary").and_then(Value::as_str).unwrap_or("").to_string();
    let mut tasks = vec![];
    for t in a.get("tasks").and_then(Value::as_array).into_iter().flatten() {
        let id = t.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let instructions = t.get("instructions").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("タスクの id「{id}」は英数字と _ だけにしてください（図の節点と同じ id）"));
        }
        if instructions.is_empty() {
            return Err(format!("タスク {id} に instructions がありません"));
        }
        if tasks.iter().any(|x: &Task| x.id == id) {
            return Err(format!("タスクの id {id} が重なっています"));
        }
        tasks.push(Task {
            title: t.get("title").and_then(Value::as_str).unwrap_or(&id).trim().to_string(),
            id,
            instructions,
            depends_on: strings(t.get("depends_on")),
            files: strings(t.get("files")),
        });
    }
    if revise.is_some() {
        return Ok(DelegateArgs { plan, summary, tasks: vec![], revise });
    }
    if tasks.is_empty() {
        return Err("tasks が空です（大きな依頼を小さなタスクに分けて渡します）".into());
    }
    if tasks.len() > max_tasks {
        return Err(format!("タスクは 1 回に {max_tasks} 個までです（{} 個ありました）。まとめ直すか、2 回に分けてください", tasks.len()));
    }
    let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
    if !plan.trim().is_empty() {
        let Some(graph) = PlanGraph::parse(&plan) else {
            return Err("plan の Mermaid 図が読めません（flowchart TD と、id[\"題\"] の節点、a --> b の矢印で書きます）".into());
        };
        for id in graph.ids() {
            if !ids.iter().any(|t| t == id) {
                return Err(format!("図の節点 {id} に対応するタスクがありません（図とタスクで同じ id を使います）"));
            }
        }
        for t in tasks.iter_mut() {
            for d in graph.waits_for(&t.id) {
                if !t.depends_on.contains(&d) {
                    t.depends_on.push(d);
                }
            }
        }
    }
    for t in &tasks {
        for d in &t.depends_on {
            if !ids.contains(d) {
                return Err(format!("タスク {} が待つ {d} がタスクにありません", t.id));
            }
        }
    }
    let edges: Vec<(String, String)> = tasks.iter().flat_map(|t| t.depends_on.iter().map(move |d| (d.clone(), t.id.clone()))).collect();
    let g = PlanGraph { labels: tasks.iter().map(|t| (t.id.clone(), t.title.clone())).collect(), edges };
    if let Some(cycle) = g.cycle() {
        return Err(format!("タスクの依存が輪になっています（{}）。順番が決まるように書き直してください", cycle.join(" → ")));
    }
    let plan = if plan.trim().is_empty() {
        PlanGraph::from_tasks(&tasks.iter().map(|t| (t.id.clone(), t.title.clone(), t.depends_on.clone())).collect::<Vec<_>>())
    } else {
        plan
    };
    Ok(DelegateArgs { plan, summary, tasks, revise: None })
}

/// The text of a developer's question, or None when [answer] is not one.
pub fn question_in(answer: &str) -> Option<String> {
    static Q: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)^question\s*[:：]\s*(.+)$").unwrap());
    Q.captures(answer.trim()).map(|c| c[1].trim().to_string()).filter(|q| !q.is_empty())
}

/// The files a developer wrote: a `File: path` line, then a fenced block.
pub fn files_in(answer: &str) -> Vec<(String, String)> {
    static FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:\*\*)?File\s*[:：]\s*`?([^\s`*]+)`?(?:\*\*)?\s*$").unwrap());
    let lines: Vec<&str> = answer.lines().collect();
    let mut out: Vec<(String, String)> = vec![];
    let mut i = 0;
    while i < lines.len() {
        if let Some(c) = FILE.captures(lines[i]) {
            let path = c[1].to_string();
            // the next fence, within a few lines
            let mut j = i + 1;
            while j < lines.len() && j <= i + 3 && !lines[j].trim_start().starts_with("```") {
                j += 1;
            }
            if j < lines.len() && lines[j].trim_start().starts_with("```") {
                let fence: String = lines[j].trim_start().chars().take_while(|c| *c == '`').collect();
                let mut body = vec![];
                let mut k = j + 1;
                while k < lines.len() && !(lines[k].trim_start().starts_with(&fence) && lines[k].trim().chars().all(|c| c == '`')) {
                    body.push(lines[k]);
                    k += 1;
                }
                let mut content = body.join("\n");
                content.push('\n');
                out.retain(|(p, _)| *p != path);
                out.push((path, content));
                i = k + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// What a developer is asked for one job; [given]: (result id, title, answer) of the jobs it waited for.
pub fn developer_task(summary: &str, task: &Task, given: &[(String, String, String)]) -> String {
    let mut b = String::new();
    if !summary.trim().is_empty() {
        b.push_str(&format!("# The plan (shared by every task)\n{}\n\n", summary.trim()));
    }
    for (id, title, answer) in given {
        b.push_str(&format!("# Result of the job you waited for: {id} — {title}\n{}\n\n", answer.trim()));
    }
    b.push_str(&format!("# Your task: {}\n{}", task.title, task.instructions.trim()));
    if !task.files.is_empty() {
        b.push_str(&format!("\n\nFiles to write: {}", task.files.join(", ")));
    }
    b
}

/// One developer model, ready to be asked.
#[derive(Clone)]
pub struct Developer {
    pub reference: String,
    pub provider: ProviderConfig,
    pub model: String,
    pub api_key: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct JobResult {
    /// d1, d2 … (what revise names)
    pub id: String,
    pub task: Task,
    pub answer: String,
    pub ok: bool,
    pub model: String,
    pub conversation: Vec<Message>,
    pub summary: String,
}

/// The results so far (for revise), at most 200.
#[derive(Default)]
pub struct Store {
    pub results: Vec<JobResult>,
    pub next: usize,
}

impl Store {
    fn take_id(&mut self) -> String {
        self.next += 1;
        format!("d{}", self.next)
    }
    fn keep(&mut self, r: JobResult) {
        self.results.push(r);
        if self.results.len() > 200 {
            self.results.remove(0);
        }
    }
    pub fn get(&self, id: &str) -> Option<&JobResult> {
        self.results.iter().find(|r| r.id == id)
    }
}

/// Progress for the screen.
pub trait Progress: Send + Sync {
    fn status(&self, done: usize, total: usize, working: &[String]);
    fn finished(&self, result: &JobResult);
    fn request(&self);
}

pub struct Limits {
    pub parallel: usize,
    pub idle_timeout: Duration,
}

async fn ask(
    client: reqwest::Client,
    dev: Developer,
    conversation: Vec<Message>,
    cancel: CancellationToken,
    idle: Duration,
) -> Result<String, String> {
    let req = ChatRequest {
        provider: &dev.provider,
        model: &dev.model,
        api_key: dev.api_key.clone(),
        system: prompt::DEVELOPER.to_string(),
        messages: &conversation,
        tools: &[],
        temperature: dev.temperature,
        top_p: None,
        max_tokens: dev.max_tokens,
        extra: Map::new(),
        cache_key: None,
        pass_reasoning: false,
    };
    let mut ignore = |_e: llm::StreamEvent| {};
    match llm::stream_chat(&client, &req, &mut ignore, &cancel, idle).await {
        Ok(c) if !c.text.trim().is_empty() => Ok(c.text),
        Ok(c) if c.finish_reason.as_deref() == Some("length") => {
            Err("思考だけで出力の上限に達しました（最大トークンを増やしてください）".into())
        }
        Ok(_) => Err("空の答えでした".into()),
        Err(e) => Err(e.message),
    }
}

struct Running {
    task: usize,
    dev: usize,
    asked_to_decide: bool,
    conversation: Vec<Message>,
}

/// Runs the jobs of [args] on [devs]; the results in task order.
pub async fn run(
    args: &DelegateArgs,
    devs: &[Developer],
    store: &mut Store,
    client: &reqwest::Client,
    cancel: &CancellationToken,
    limits: &Limits,
    progress: &dyn Progress,
) -> Vec<JobResult> {
    if let Some((id, feedback)) = &args.revise {
        return revise(id, feedback, devs, store, client, cancel, limits, progress).await.into_iter().collect();
    }
    let n = args.tasks.len();
    let mut results: Vec<Option<JobResult>> = vec![None; n];
    let mut tried: Vec<Vec<usize>> = vec![vec![]; n];
    let mut started = vec![false; n];
    let mut active = vec![0usize; devs.len()];
    let mut set: JoinSet<(Running, Result<String, String>)> = JoinSet::new();
    let index: HashMap<&str, usize> = args.tasks.iter().enumerate().map(|(i, t)| (t.id.as_str(), i)).collect();
    let cap = limits.parallel.max(1);

    loop {
        // start what can start
        for i in 0..n {
            if started[i] {
                continue;
            }
            let deps: Vec<usize> = args.tasks[i].depends_on.iter().filter_map(|d| index.get(d.as_str()).copied()).collect();
            if deps.iter().any(|d| results[*d].is_none()) {
                continue;
            }
            if let Some(failed) = deps.iter().find(|d| !results[**d].as_ref().unwrap().ok) {
                let f = results[*failed].as_ref().unwrap();
                started[i] = true;
                let r = JobResult {
                    id: store.take_id(),
                    task: args.tasks[i].clone(),
                    answer: format!("前の仕事 {}（{}）が失敗したため、実行しませんでした", f.id, f.task.title),
                    ok: false,
                    model: String::new(),
                    conversation: vec![],
                    summary: args.summary.clone(),
                };
                progress.finished(&r);
                results[i] = Some(r);
                continue;
            }
            let Some(dev) = (0..devs.len()).filter(|d| active[*d] < cap && !tried[i].contains(d)).min_by_key(|d| active[*d]) else {
                continue;
            };
            let given: Vec<(String, String, String)> = deps
                .iter()
                .map(|d| {
                    let r = results[*d].as_ref().unwrap();
                    (r.id.clone(), r.task.title.clone(), r.answer.clone())
                })
                .collect();
            let conversation = vec![Message::user(developer_task(&args.summary, &args.tasks[i], &given))];
            started[i] = true;
            tried[i].push(dev);
            active[dev] += 1;
            progress.request();
            let run = Running { task: i, dev, asked_to_decide: false, conversation: conversation.clone() };
            let (c, d, k, idle) = (client.clone(), devs[dev].clone(), cancel.clone(), limits.idle_timeout);
            set.spawn(async move { (run, ask(c, d, conversation, k, idle).await) });
        }
        let done = results.iter().filter(|r| r.is_some()).count();
        if done == n {
            break;
        }
        let working: Vec<String> = (0..n).filter(|i| started[*i] && results[*i].is_none()).map(|i| args.tasks[i].title.clone()).collect();
        progress.status(done, n, &working);
        let joined = tokio::select! {
            _ = cancel.cancelled() => { set.abort_all(); break; }
            j = set.join_next() => j,
        };
        let Some(Ok((mut run, answer))) = joined else {
            if set.is_empty() {
                break;
            }
            continue;
        };
        active[run.dev] -= 1;
        let i = run.task;
        let finish = |ok: bool, answer: String, run: &Running, store: &mut Store| JobResult {
            id: store.take_id(),
            task: args.tasks[run.task].clone(),
            answer,
            ok,
            model: devs[run.dev].reference.clone(),
            conversation: run.conversation.clone(),
            summary: args.summary.clone(),
        };
        match answer {
            Ok(text) => match question_in(&text) {
                Some(q) if !run.asked_to_decide => {
                    run.conversation.push(Message::Assistant { content: text, reasoning: None, tool_calls: vec![] });
                    run.conversation.push(Message::user(prompt::DECIDE_YOURSELF));
                    run.asked_to_decide = true;
                    active[run.dev] += 1;
                    progress.request();
                    let _ = q;
                    let conv = run.conversation.clone();
                    let (c, d, k, idle) = (client.clone(), devs[run.dev].clone(), cancel.clone(), limits.idle_timeout);
                    set.spawn(async move { (run, ask(c, d, conv, k, idle).await) });
                }
                Some(q) => {
                    let r = finish(false, format!("開発モデルが質問を返しました（決めるよう伝えたあとも）: {q}"), &run, store);
                    progress.finished(&r);
                    results[i] = Some(r);
                }
                None => {
                    run.conversation.push(Message::Assistant { content: text.clone(), reasoning: None, tool_calls: vec![] });
                    let r = finish(true, text, &run, store);
                    progress.finished(&r);
                    results[i] = Some(r);
                }
            },
            Err(e) => {
                // the job goes to a model it has not tried, once
                let other = (0..devs.len()).filter(|d| !tried[i].contains(d)).min_by_key(|d| active[*d]);
                match other {
                    Some(dev) if tried[i].len() < 2 => {
                        tried[i].push(dev);
                        active[dev] += 1;
                        progress.request();
                        let conv = vec![run.conversation[0].clone()];
                        let next = Running { task: i, dev, asked_to_decide: false, conversation: conv.clone() };
                        let (c, d, k, idle) = (client.clone(), devs[dev].clone(), cancel.clone(), limits.idle_timeout);
                        set.spawn(async move { (next, ask(c, d, conv, k, idle).await) });
                    }
                    _ => {
                        let r = finish(false, format!("失敗しました: {e}"), &run, store);
                        progress.finished(&r);
                        results[i] = Some(r);
                    }
                }
            }
        }
    }
    let out: Vec<JobResult> = results.into_iter().flatten().collect();
    for r in &out {
        store.keep(r.clone());
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn revise(
    id: &str,
    feedback: &str,
    devs: &[Developer],
    store: &mut Store,
    client: &reqwest::Client,
    cancel: &CancellationToken,
    limits: &Limits,
    progress: &dyn Progress,
) -> Option<JobResult> {
    let earlier = store.get(id)?.clone();
    let dev = devs.iter().position(|d| d.reference == earlier.model).unwrap_or(0);
    let mut conversation = earlier.conversation.clone();
    if conversation.is_empty() {
        conversation.push(Message::user(developer_task(&earlier.summary, &earlier.task, &[])));
        conversation.push(Message::Assistant { content: earlier.answer.clone(), reasoning: None, tool_calls: vec![] });
    }
    conversation.push(Message::user(prompt::correction(feedback)));
    progress.request();
    progress.status(0, 1, std::slice::from_ref(&earlier.task.title));
    let answer = ask(client.clone(), devs[dev].clone(), conversation.clone(), cancel.clone(), limits.idle_timeout).await;
    let (ok, text) = match answer {
        Ok(t) => (true, t),
        Err(e) => (false, format!("失敗しました: {e}")),
    };
    if ok {
        conversation.push(Message::Assistant { content: text.clone(), reasoning: None, tool_calls: vec![] });
    }
    let r = JobResult {
        id: store.take_id(),
        task: earlier.task.clone(),
        answer: text,
        ok,
        model: devs[dev].reference.clone(),
        conversation,
        summary: earlier.summary.clone(),
    };
    progress.finished(&r);
    store.keep(r.clone());
    Some(r)
}

/// What the planner reads back: each result with its id, cut to [chars].
pub fn report(results: &[JobResult], chars: usize, written: &BTreeMap<String, Vec<String>>) -> String {
    if results.is_empty() {
        return "結果がありません（止めたか、revise の id が見つかりません）".into();
    }
    let mut b = String::new();
    for r in results {
        let state = if r.ok { "ok" } else { "failed" };
        b.push_str(&format!(
            "## {} — {} (task {}, {state}, {})\n",
            r.id,
            r.task.title,
            r.task.id,
            if r.model.is_empty() { "-" } else { &r.model }
        ));
        if let Some(files) = written.get(&r.id)
            && !files.is_empty()
        {
            b.push_str(&format!("Files written: {}\n", files.join(", ")));
        }
        b.push_str(&crate::tools::clip_middle(r.answer.trim(), chars));
        b.push_str("\n\n");
    }
    if results.iter().all(|r| !r.ok) {
        b.push_str("Every job failed. Do the work yourself, or split it into simpler jobs.\n");
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn parse_joins_the_graph_and_depends_on() {
        let a = args(json!({
            "plan": "flowchart TD\n a[\"Model\"] --> c[\"Screen\"]\n b[\"API\"]",
            "tasks": [
                {"id":"a","title":"Model","instructions":"x"},
                {"id":"b","title":"API","instructions":"y"},
                {"id":"c","title":"Screen","instructions":"z","depends_on":["b"]}
            ]
        }));
        let d = parse(&a, 8).unwrap();
        assert_eq!(d.tasks[2].depends_on, vec!["b", "a"]);
    }

    #[test]
    fn parse_refuses_what_cannot_run() {
        let cyc =
            args(json!({"tasks":[{"id":"a","instructions":"x","depends_on":["b"]},{"id":"b","instructions":"y","depends_on":["a"]}]}));
        assert!(parse(&cyc, 8).unwrap_err().contains("輪"));
        let unknown = args(json!({"plan":"flowchart TD\nq --> a","tasks":[{"id":"a","instructions":"x"}]}));
        assert!(parse(&unknown, 8).unwrap_err().contains("節点 q"));
        let many = args(json!({"tasks":[{"id":"a","instructions":"x"},{"id":"b","instructions":"y"}]}));
        assert!(parse(&many, 1).unwrap_err().contains("1 個まで"));
        let empty = args(json!({"tasks":[{"id":"a","instructions":""}]}));
        assert!(parse(&empty, 8).is_err());
        let revise = args(json!({"revise":{"id":"d2","feedback":"fix"}}));
        assert_eq!(parse(&revise, 8).unwrap().revise, Some(("d2".into(), "fix".into())));
    }

    #[test]
    fn questions_and_files_are_read() {
        assert_eq!(question_in("QUESTION: which db?"), Some("which db?".into()));
        assert_eq!(question_in("here is code\nQUESTION: x"), None);
        let ans = "File: src/a.rs\n```rust\nfn a() {}\n```\nsome text\n**File: `web/index.html`**\n\n```html\n<p>x</p>\n```\n";
        assert_eq!(files_in(ans), vec![("src/a.rs".into(), "fn a() {}\n".into()), ("web/index.html".into(), "<p>x</p>\n".into())]);
    }

    #[test]
    fn developer_task_carries_what_it_waited_for() {
        let t = Task { id: "b".into(), title: "B".into(), instructions: "do".into(), depends_on: vec![], files: vec!["b.rs".into()] };
        let s = developer_task("shared", &t, &[("d1".into(), "A".into(), "fn a()".into())]);
        assert!(s.starts_with("# The plan (shared by every task)\nshared"));
        assert!(s.contains("# Result of the job you waited for: d1 — A\nfn a()"));
        assert!(s.ends_with("Files to write: b.rs"));
    }
}
