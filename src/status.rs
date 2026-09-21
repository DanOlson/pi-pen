//! `pi-sandbox status`: a progress view over headless run directories.
//!
//! Everything here is derived from artifacts the launcher already writes
//! (`run.json`, `events.jsonl`, `result.json`, `stderr.log`), so no launcher
//! change is needed and the run contract is untouched. Pi events carry no
//! timestamps, so "how long has this been quiet" comes from the modification
//! time of `events.jsonl` rather than from the events themselves.

use crate::headless::{docker_capture_controlled, final_assistant_text};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SUMMARY_WIDTH: usize = 48;
const TEXT_WIDTH: usize = 60;

#[derive(Debug)]
pub struct Options {
    pub target: Target,
    pub watch: bool,
    pub json: bool,
    pub inspect: bool,
    pub interval: u64,
}

#[derive(Debug)]
pub enum Target {
    Run(PathBuf),
    Batch(PathBuf),
}

/// One run directory, as far as its artifacts can tell.
#[derive(Debug, Serialize)]
pub struct RunStatus {
    pub name: String,
    pub path: PathBuf,
    /// `run.json` state, or `missing` when the directory has no record yet.
    pub state: String,
    pub mode: Option<String>,
    /// Terminal outcome from `result.json`, when it exists.
    pub status: Option<String>,
    pub reason: Option<String>,
    pub wrapper_exit_code: Option<i32>,
    pub container_exit_code: Option<i32>,
    pub cleanup_succeeded: Option<bool>,
    pub elapsed_ms: u128,
    /// Time since `events.jsonl` was last written; only meaningful while running.
    pub idle_ms: Option<u128>,
    pub turns: u64,
    pub tool_calls: u64,
    /// `tool`, `model`, `done`, or `starting`; absent when there are no events.
    pub phase: Option<String>,
    pub tool: Option<ToolCall>,
    pub pending_tools: usize,
    pub last_text: Option<String>,
    pub stderr_tail: Option<String>,
    /// `result.json.error`: the classified failure, when the run stopped short.
    pub error: Option<FailureSummary>,
    pub container: Option<ContainerState>,
}

#[derive(Debug, Serialize, Clone)]
pub struct FailureSummary {
    pub class: String,
    pub message: String,
    pub recoverable: bool,
    pub recovery: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ToolCall {
    pub name: String,
    pub summary: String,
}

#[derive(Debug, Serialize)]
pub struct ContainerState {
    pub status: String,
    pub exit_code: Option<i32>,
}

pub fn run(options: Options, cwd: &Path) -> Result<()> {
    let target = match options.target {
        Target::Run(path) => Target::Run(absolute(&path, cwd)),
        Target::Batch(path) => Target::Batch(absolute(&path, cwd)),
    };
    if !options.watch {
        let runs = collect(&target, options.inspect)?;
        return print_snapshot(&target, &runs, options.json);
    }
    let interval = Duration::from_secs(options.interval.max(1));
    let clear = !options.json && std::io::stdout().is_terminal();
    loop {
        let runs = collect(&target, options.inspect)?;
        let mut out = std::io::stdout().lock();
        if clear {
            out.write_all(b"\x1b[2J\x1b[H")?;
        }
        if options.json {
            writeln!(out, "{}", serde_json::to_string(&runs)?)?;
        } else {
            out.write_all(render_table(&runs).as_bytes())?;
            let active = runs.iter().filter(|r| !is_terminal(r)).count();
            writeln!(
                out,
                "\n{active} of {} still running; refreshing every {}s (Ctrl-C to exit)",
                runs.len(),
                interval.as_secs()
            )?;
        }
        out.flush()?;
        drop(out);
        if runs.iter().all(is_terminal) {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

fn print_snapshot(target: &Target, runs: &[RunStatus], json: bool) -> Result<()> {
    if json {
        match target {
            Target::Run(_) => println!("{}", serde_json::to_string_pretty(&runs[0])?),
            Target::Batch(_) => println!("{}", serde_json::to_string_pretty(runs)?),
        }
    } else {
        print!("{}", render_table(runs));
    }
    Ok(())
}

fn is_terminal(run: &RunStatus) -> bool {
    run.state == "terminal"
}

pub fn collect(target: &Target, inspect: bool) -> Result<Vec<RunStatus>> {
    let dirs = match target {
        Target::Run(dir) => {
            if !dir.join("run.json").is_file() {
                bail!("{} has no run.json; is it a run directory?", dir.display());
            }
            vec![dir.clone()]
        }
        Target::Batch(batch) => {
            let mut dirs: Vec<PathBuf> = fs::read_dir(batch)
                .with_context(|| format!("failed to read {}", batch.display()))?
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|path| path.join("run.json").is_file())
                .collect();
            if dirs.is_empty() {
                bail!("no run directories under {}", batch.display());
            }
            dirs.sort();
            dirs
        }
    };
    Ok(dirs
        .iter()
        .map(|dir| read_run_status(dir, inspect))
        .collect())
}

fn read_run_status(dir: &Path, inspect: bool) -> RunStatus {
    let name = dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let now = now_ms();
    let record = read_json(&dir.join("run.json"));
    let result = read_json(&dir.join("result.json"));
    let state = record
        .as_ref()
        .and_then(|r| r.get("state")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "missing".into());
    let created = record
        .as_ref()
        .and_then(|r| r.get("created_at_unix_ms")?.as_u64().map(u128::from));
    let updated = record
        .as_ref()
        .and_then(|r| r.get("updated_at_unix_ms")?.as_u64().map(u128::from));
    let field = |key: &str| result.as_ref().and_then(|r| r.get(key).cloned());
    let elapsed_ms = match (
        field("started_at_unix_ms").and_then(|v| v.as_u64()),
        field("finished_at_unix_ms").and_then(|v| v.as_u64()),
    ) {
        (Some(started), Some(finished)) => u128::from(finished.saturating_sub(started)),
        _ if state == "terminal" => updated
            .unwrap_or(now)
            .saturating_sub(created.unwrap_or(now)),
        _ => now.saturating_sub(created.unwrap_or(now)),
    };
    let events_path = dir.join("events.jsonl");
    let idle_ms = (state != "terminal").then(|| {
        let last_write = fs::metadata(&events_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or_else(|| updated.unwrap_or(now), |d| d.as_millis());
        now.saturating_sub(last_write)
    });
    let events = scan_events(&events_path);
    let status = field("status").and_then(|v| v.as_str().map(str::to_owned));
    let error = field("error").and_then(|value| {
        Some(FailureSummary {
            class: value.get("class")?.as_str()?.to_owned(),
            message: value.get("message")?.as_str()?.to_owned(),
            recoverable: value
                .get("recoverable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            recovery: value
                .get("recovery")
                .and_then(Value::as_str)
                .unwrap_or("escalate")
                .to_owned(),
        })
    });
    let stderr_tail = if status.as_deref().is_some_and(|s| s != "completed") {
        // The classified error already quotes the stderr tail for worker
        // exits and interrupted streams; do not print it twice.
        last_line(&dir.join("stderr.log"))
            .filter(|line| error.as_ref().is_none_or(|e| e.message != *line))
    } else {
        None
    };
    let container = if inspect && state != "terminal" {
        record
            .as_ref()
            .and_then(|r| r.get("container_id")?.as_str())
            .map(inspect_container)
    } else {
        None
    };
    RunStatus {
        name,
        path: dir.to_path_buf(),
        mode: record
            .as_ref()
            .and_then(|r| r.get("mode")?.as_str().map(str::to_owned)),
        state,
        status,
        reason: field("reason").and_then(|v| v.as_str().map(str::to_owned)),
        wrapper_exit_code: field("wrapper_exit_code").and_then(|v| v.as_i64()?.try_into().ok()),
        container_exit_code: field("container_exit_code").and_then(|v| v.as_i64()?.try_into().ok()),
        cleanup_succeeded: field("cleanup_succeeded").and_then(|v| v.as_bool()),
        elapsed_ms,
        idle_ms,
        turns: events.turns,
        tool_calls: events.tool_calls,
        phase: events.phase(),
        tool: events.pending.last().map(|(_, call)| call.clone()),
        pending_tools: events.pending.len(),
        last_text: events.last_text,
        stderr_tail,
        error,
        container,
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn last_line(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| truncate(&collapse(line), TEXT_WIDTH))
}

fn inspect_container(id: &str) -> ContainerState {
    let signal = AtomicUsize::new(0);
    let deadline = Some(Instant::now() + Duration::from_secs(5));
    match docker_capture_controlled(
        &[
            "inspect",
            "--format",
            "{{.State.Status}} {{.State.ExitCode}}",
            id,
        ],
        deadline,
        &signal,
    ) {
        Ok(output) => {
            let mut parts = output.split_whitespace();
            ContainerState {
                status: parts.next().unwrap_or("unknown").to_string(),
                exit_code: parts.next().and_then(|c| c.parse().ok()),
            }
        }
        Err(_) => ContainerState {
            status: "missing".into(),
            exit_code: None,
        },
    }
}

/// What the significant events say about where the agent is. Token deltas and
/// partial tool output are skipped before JSON parsing, since they dominate
/// the file and carry nothing this view needs.
#[derive(Debug, Default)]
struct EventSummary {
    turns: u64,
    tool_calls: u64,
    /// Tool calls that started and have not ended, in start order.
    pending: Vec<(String, ToolCall)>,
    last_text: Option<String>,
    saw_events: bool,
    agent_active: bool,
    agent_ended: bool,
}

impl EventSummary {
    fn phase(&self) -> Option<String> {
        if !self.saw_events {
            return None;
        }
        Some(
            if !self.pending.is_empty() {
                "tool"
            } else if self.agent_active {
                "model"
            } else if self.agent_ended {
                "done"
            } else {
                "starting"
            }
            .into(),
        )
    }

    fn apply(&mut self, event: &Value) {
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return;
        };
        self.saw_events = true;
        match kind {
            "agent_start" => {
                self.agent_active = true;
                self.agent_ended = false;
            }
            "agent_end" => {
                self.agent_active = false;
                self.agent_ended = true;
                self.pending.clear();
            }
            "turn_start" => self.turns += 1,
            "tool_execution_start" => {
                self.tool_calls += 1;
                let id = event
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = event
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let summary = summarize_args(&name, event.get("args"));
                self.pending.push((id, ToolCall { name, summary }));
            }
            "tool_execution_end" => {
                let id = event.get("toolCallId").and_then(Value::as_str);
                match self
                    .pending
                    .iter()
                    .rposition(|(pending, _)| Some(pending.as_str()) == id)
                {
                    Some(index) => {
                        self.pending.remove(index);
                    }
                    None => {
                        self.pending.pop();
                    }
                }
            }
            "message_end" => {
                if let Some(text) = final_assistant_text(event) {
                    let text = collapse(&text);
                    if !text.is_empty() {
                        self.last_text = Some(truncate(&text, TEXT_WIDTH));
                    }
                }
            }
            _ => {}
        }
    }
}

fn scan_events(path: &Path) -> EventSummary {
    let mut summary = EventSummary::default();
    let Ok(file) = File::open(path) else {
        return summary;
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.contains("\"type\":\"message_update\"")
            || line.contains("\"type\":\"tool_execution_update\"")
            || line.contains("\"type\":\"message_start\"")
        {
            continue;
        }
        // A line the launcher is still writing does not parse; skip it.
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Exec wraps pi events in envelopes; rpc writes them raw.
        let event = record.get("event").unwrap_or(&record);
        summary.apply(event);
    }
    summary
}

fn summarize_args(tool: &str, args: Option<&Value>) -> String {
    let Some(Value::Object(args)) = args else {
        return String::new();
    };
    let preferred: &[&str] = match tool {
        "bash" => &["command"],
        _ => &["path", "file_path", "command", "pattern", "url", "query"],
    };
    let value = preferred
        .iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .or_else(|| args.values().find_map(Value::as_str))
        .unwrap_or_default();
    truncate(&collapse(value), SUMMARY_WIDTH)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(text: &str, width: usize) -> String {
    let mut chars = text.char_indices();
    match chars.nth(width) {
        Some((end, _)) => format!("{}…", text[..end].trim_end()),
        None => text.to_string(),
    }
}

pub fn render_table(runs: &[RunStatus]) -> String {
    let width = runs
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for run in runs {
        let verb = run.status.as_deref().unwrap_or(&run.state);
        out.push_str(&format!(
            "{:<width$}  {:<10} {:>7}",
            run.name,
            verb,
            format_duration(run.elapsed_ms)
        ));
        for detail in describe(run) {
            out.push_str("  ");
            out.push_str(&detail);
        }
        out.push('\n');
    }
    out
}

fn describe(run: &RunStatus) -> Vec<String> {
    let mut details = Vec::new();
    if run.state == "missing" {
        details.push("no run.json".into());
        return details;
    }
    if let Some(status) = &run.status {
        if let Some(code) = run.wrapper_exit_code {
            details.push(format!("exit {code}"));
        }
        if status != "completed"
            && let Some(reason) = &run.reason
        {
            details.push(reason.clone());
        }
        if let Some(error) = &run.error {
            details.push(format!(
                "{} ({}): {}",
                error.class,
                error.recovery.replace('_', " "),
                truncate(&error.message, TEXT_WIDTH)
            ));
        }
        match run.cleanup_succeeded {
            Some(true) => details.push("cleanup ok".into()),
            Some(false) => details.push("cleanup FAILED".into()),
            None => {}
        }
        if let Some(line) = &run.stderr_tail {
            details.push(line.clone());
        }
        return details;
    }
    if run.state == "terminal" {
        details.push("no result.json".into());
        return details;
    }
    match run.phase.as_deref() {
        Some("tool") => {
            if let Some(tool) = &run.tool {
                let mut text = format!("{}: {}", tool.name, tool.summary);
                if run.pending_tools > 1 {
                    text.push_str(&format!(" (+{} more)", run.pending_tools - 1));
                }
                details.push(text);
            }
        }
        Some("model") => details.push("awaiting model".into()),
        Some("done") => details.push("agent finished".into()),
        Some(_) => details.push("session starting".into()),
        None => details.push("no events yet".into()),
    }
    if let Some(idle) = run.idle_ms {
        details.push(format!("idle {}", format_duration(idle)));
    }
    if let Some(text) = &run.last_text {
        details.push(format!("last: \"{text}\""));
    }
    if let Some(container) = &run.container {
        details.push(match (container.status.as_str(), container.exit_code) {
            ("running", _) => "container alive".into(),
            ("missing", _) => "container MISSING".into(),
            (status, Some(code)) => format!("container {status} ({code})"),
            (status, None) => format!("container {status}"),
        });
    }
    details
}

pub fn format_duration(ms: u128) -> String {
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else {
        format!("{minutes}m{seconds:02}s")
    }
}

fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn write_events(dir: &Path, events: &[Value]) {
        let mut lines = String::new();
        for (i, event) in events.iter().enumerate() {
            let envelope =
                json!({"schema_version":1,"type":"pi_event","run_id":"r","seq":i+1,"event":event});
            lines.push_str(&envelope.to_string());
            lines.push('\n');
        }
        fs::write(dir.join("events.jsonl"), lines).unwrap();
    }

    fn write_run(dir: &Path, state: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("run.json"),
            json!({"state":state,"mode":"exec","created_at_unix_ms":now_ms()-90_000,"updated_at_unix_ms":now_ms(),"container_id":"abc"}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn running_worker_shows_current_tool_and_last_text() {
        let root = tempdir().unwrap();
        let dir = root.path().join("implement-01");
        write_run(&dir, "running");
        write_events(
            &dir,
            &[
                json!({"type":"agent_start"}),
                json!({"type":"turn_start"}),
                json!({"type":"message_update","message":{"role":"assistant","content":"partial"}}),
                json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"Adding a  regression\ntest first"}]}}),
                json!({"type":"tool_execution_start","toolCallId":"t1","toolName":"bash","args":{"command":"cargo test --lib"}}),
                json!({"type":"tool_execution_update","toolCallId":"t1","toolName":"bash","args":{},"partialResult":"..."}),
            ],
        );
        let runs = collect(&Target::Batch(root.path().to_path_buf()), false).unwrap();
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.state, "running");
        assert_eq!(run.phase.as_deref(), Some("tool"));
        assert_eq!(run.tool.as_ref().unwrap().summary, "cargo test --lib");
        assert_eq!(run.turns, 1);
        assert_eq!(run.tool_calls, 1);
        assert_eq!(
            run.last_text.as_deref(),
            Some("Adding a regression test first")
        );
        assert!(run.idle_ms.is_some());
        let table = render_table(&runs);
        assert!(table.starts_with("implement-01  running    "), "{table}");
        assert!(table.contains("bash: cargo test --lib"));
        assert!(table.contains("last: \"Adding a regression test first\""));
    }

    #[test]
    fn finished_tool_leaves_worker_awaiting_model() {
        let root = tempdir().unwrap();
        let dir = root.path().join("run-01");
        write_run(&dir, "running");
        write_events(
            &dir,
            &[
                json!({"type":"agent_start"}),
                json!({"type":"tool_execution_start","toolCallId":"a","toolName":"read","args":{"path":"src/lib.rs"}}),
                json!({"type":"tool_execution_start","toolCallId":"b","toolName":"bash","args":{"command":"ls"}}),
                json!({"type":"tool_execution_end","toolCallId":"a","toolName":"read","result":{},"isError":false}),
            ],
        );
        let run = read_run_status(&dir, false);
        assert_eq!(run.pending_tools, 1);
        assert_eq!(run.tool.as_ref().unwrap().name, "bash");
        write_events(
            &dir,
            &[
                json!({"type":"agent_start"}),
                json!({"type":"tool_execution_start","toolCallId":"a","toolName":"read","args":{"path":"src/lib.rs"}}),
                json!({"type":"tool_execution_end","toolCallId":"a","toolName":"read","result":{},"isError":false}),
            ],
        );
        let run = read_run_status(&dir, false);
        assert_eq!(run.phase.as_deref(), Some("model"));
        assert!(render_table(&[run]).contains("awaiting model"));
    }

    #[test]
    fn terminal_runs_report_result_and_stderr() {
        let root = tempdir().unwrap();
        let ok = root.path().join("review-01");
        write_run(&ok, "terminal");
        fs::write(
            ok.join("result.json"),
            json!({"status":"completed","reason":"normal_exit","wrapper_exit_code":0,"container_exit_code":0,"started_at_unix_ms":1000,"finished_at_unix_ms":366_000,"cleanup_succeeded":true}).to_string(),
        )
        .unwrap();
        let failed = root.path().join("review-02");
        write_run(&failed, "terminal");
        fs::write(
            failed.join("result.json"),
            json!({"status":"failed","reason":"worker_exit","wrapper_exit_code":1,"container_exit_code":1,"started_at_unix_ms":0,"finished_at_unix_ms":3000,"cleanup_succeeded":true}).to_string(),
        )
        .unwrap();
        fs::write(
            failed.join("stderr.log"),
            "warning: no saved pi credentials in the persistent home\n\n",
        )
        .unwrap();
        let runs = collect(&Target::Batch(root.path().to_path_buf()), false).unwrap();
        let table = render_table(&runs);
        assert!(
            table.contains("review-01  completed    6m05s  exit 0  cleanup ok\n"),
            "{table}"
        );
        assert!(
            table.contains("review-02  failed       0m03s  exit 1  worker_exit  cleanup ok  warning: no saved pi credentials"),
            "{table}"
        );
        assert!(runs.iter().all(|r| r.idle_ms.is_none()));
    }

    #[test]
    fn classified_errors_replace_the_stderr_tail() {
        let root = tempdir().unwrap();
        let dir = root.path().join("implement-01");
        write_run(&dir, "terminal");
        fs::write(
            dir.join("result.json"),
            json!({"status":"failed","reason":"agent_error","wrapper_exit_code":1,"container_exit_code":0,"started_at_unix_ms":0,"finished_at_unix_ms":3000,"cleanup_succeeded":true,
                "error":{"class":"provider_capacity","message":"Selected model is at capacity. Please try a different model.","recoverable":true,"recovery":"resume"}}).to_string(),
        )
        .unwrap();
        fs::write(dir.join("stderr.log"), "context canceled\n").unwrap();
        let run = read_run_status(&dir, false);
        let table = render_table(std::slice::from_ref(&run));
        assert!(
            table.contains(
                "agent_error  provider_capacity (resume): Selected model is at capacity."
            ),
            "{table}"
        );
        assert!(table.contains("context canceled"), "{table}");

        let interrupted = root.path().join("implement-02");
        write_run(&interrupted, "terminal");
        fs::write(
            interrupted.join("result.json"),
            json!({"status":"failed","reason":"incomplete_event_stream","wrapper_exit_code":1,"container_exit_code":1,"started_at_unix_ms":0,"finished_at_unix_ms":3000,"cleanup_succeeded":true,
                "error":{"class":"stream_interrupted","message":"context canceled","recoverable":true,"recovery":"resume"}}).to_string(),
        )
        .unwrap();
        fs::write(interrupted.join("stderr.log"), "context canceled\n").unwrap();
        let run = read_run_status(&interrupted, false);
        assert!(run.stderr_tail.is_none());
        assert_eq!(run.error.as_ref().unwrap().class, "stream_interrupted");
    }

    #[test]
    fn rpc_events_without_envelopes_are_understood() {
        let root = tempdir().unwrap();
        let dir = root.path().join("rpc-01");
        write_run(&dir, "running");
        fs::write(
            dir.join("events.jsonl"),
            "{\"type\":\"response\",\"command\":\"prompt\",\"success\":true}\n{\"type\":\"agent_start\"}\n{\"type\":\"tool_execution_start\",\"toolCallId\":\"x\",\"toolName\":\"grep\",\"args\":{\"pattern\":\"TODO\",\"path\":\"src\"}}\n{\"type\":\"partial",
        )
        .unwrap();
        let run = read_run_status(&dir, false);
        assert_eq!(run.tool.as_ref().unwrap().summary, "src");
        assert_eq!(run.phase.as_deref(), Some("tool"));
    }

    #[test]
    fn batch_requires_run_directories() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("prompts")).unwrap();
        let error = collect(&Target::Batch(root.path().to_path_buf()), false).unwrap_err();
        assert!(error.to_string().contains("no run directories"));
        let error = collect(&Target::Run(root.path().join("prompts")), false).unwrap_err();
        assert!(error.to_string().contains("no run.json"));
    }

    #[test]
    fn durations_and_truncation_are_compact() {
        assert_eq!(format_duration(0), "0m00s");
        assert_eq!(format_duration(872_000), "14m32s");
        assert_eq!(format_duration(3_725_000), "1h02m");
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdefgh", 5), "abcde…");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }
}
