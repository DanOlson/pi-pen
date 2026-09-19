use super::{
    build_images, cache_dir, command_output, fnv1a64, persistent_home_dir, prepare_environment,
    resolve_env, seed_git_config,
};
use crate::{ContainerContext, LaunchEnvFile, RunOverrides};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const PI_VERSION: &str = "0.85.1";
pub const SCHEMA_VERSION: u32 = 1;
const MAX_PROMPT: u64 = 8 * 1024 * 1024;
const MAX_RECORD: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub message: String,
}
impl std::fmt::Display for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ExitError {}

#[derive(Debug)]
pub enum HeadlessCommand {
    Exec(Options),
    Rpc(Options),
    Stop { output_dir: PathBuf, grace: u64 },
}

#[derive(Debug)]
pub struct Options {
    pub env: Option<String>,
    pub rebuild: bool,
    pub pull: bool,
    pub output_dir: PathBuf,
    pub timeout: Option<u64>,
    pub resume_session: Option<PathBuf>,
    pub prompt: Option<Prompt>,
    pub pi_args: Vec<String>,
    pub overrides: RunOverrides,
}

#[derive(Debug)]
pub enum Prompt {
    File(PathBuf),
    Stdin,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct RunRecord {
    schema_version: u32,
    run_id: Uuid,
    mode: String,
    state: String,
    created_at_unix_ms: u128,
    updated_at_unix_ms: u128,
    cwd: PathBuf,
    output_dir: PathBuf,
    image: Option<String>,
    pi_version: String,
    container_id: Option<String>,
    docker_context: Option<String>,
    cancellation_requested: bool,
    resume_source: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct ResultRecord {
    schema_version: u32,
    run_id: Uuid,
    status: String,
    reason: String,
    wrapper_exit_code: i32,
    container_exit_code: Option<i32>,
    started_at_unix_ms: u128,
    finished_at_unix_ms: u128,
    response_available: bool,
    usage: Option<Value>,
}

pub fn run(command: HeadlessCommand, cwd: &Path) -> Result<()> {
    match command {
        HeadlessCommand::Exec(o) => execute(o, cwd, Mode::Exec),
        HeadlessCommand::Rpc(o) => execute(o, cwd, Mode::Rpc),
        HeadlessCommand::Stop { output_dir, grace } => stop(&output_dir, grace),
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum Mode {
    Exec,
    Rpc,
}
impl Mode {
    fn name(self) -> &'static str {
        if self == Self::Exec { "exec" } else { "rpc" }
    }
}

fn execute(options: Options, cwd: &Path, mode: Mode) -> Result<()> {
    let started = now_ms();
    let prompt = if mode == Mode::Exec {
        Some(read_prompt(
            options
                .prompt
                .as_ref()
                .context("exec requires a prompt source")?,
        )?)
    } else {
        if options.prompt.is_some() {
            bail!("rpc does not accept a prompt source");
        }
        None
    };
    validate_pi_args(&options.pi_args)?;
    if let Some(seconds) = options.timeout
        && seconds == 0
    {
        bail!("timeout must be positive");
    }
    let deadline = options
        .timeout
        .map(|seconds| Instant::now() + Duration::from_secs(seconds));
    let resume = prepare_resume_source(options.resume_session.as_deref())?;
    let resolved = resolve_env(options.env.as_deref(), cwd)?;
    let launch = crate::launch::resolve_run_config(&resolved, cwd, &options.overrides)?;
    let prepared = prepare_environment(&resolved, cwd)?;
    let tag = prepared
        .images
        .last()
        .context("environment produced no image")?
        .tag
        .clone();
    if options.overrides.dry_run {
        crate::launch::print_dry_run(&tag, &launch);
        return Ok(());
    }
    let requested_output = if options.output_dir.is_absolute() {
        options.output_dir.clone()
    } else {
        cwd.join(&options.output_dir)
    };
    let canonical_cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if requested_output.starts_with(&canonical_cwd) {
        bail!("output directory must be outside the mounted workspace");
    }
    let output = claim_output(&options.output_dir, cwd)?;
    let canonical_output = fs::canonicalize(&output)?;
    let _worktree_lock = lock_worktree(cwd)?;
    let run_id = Uuid::new_v4();
    let docker_context = docker_output(&["context", "show"]).ok();
    let mut record = RunRecord {
        schema_version: SCHEMA_VERSION,
        run_id,
        mode: mode.name().into(),
        state: "preparing".into(),
        created_at_unix_ms: started,
        updated_at_unix_ms: started,
        cwd: fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
        output_dir: output.clone(),
        image: None,
        pi_version: PI_VERSION.into(),
        container_id: None,
        docker_context,
        cancellation_requested: false,
        resume_source: resume.clone(),
    };
    write_json_atomic(&output.join("run.json"), &record)?;
    fs::create_dir(output.join("sessions")).context("failed to create sessions directory")?;
    if let Some(source) = resume.as_deref() {
        let name = source
            .file_name()
            .context("resume session has no filename")?;
        fs::copy(source, output.join("sessions").join(name))
            .context("failed to clone resume session")?;
    }
    let cache = cache_dir()?;
    // Build output is diagnostics. Existing build helpers currently inherit stderr/stdout;
    // the reuse notice is deliberately stderr in machine mode.
    build_images(&cache, &prepared.images, options.rebuild, options.pull)?;
    crate::launch::preflight_networks(&launch)?;
    record.image = Some(tag.clone());
    record.state = "starting".into();
    update_record(&output, &mut record)?;
    let home = persistent_home_dir(cwd, &resolved)?;
    if canonical_output.starts_with(fs::canonicalize(&home).unwrap_or_else(|_| home.clone())) {
        bail!("output directory must be outside the mounted persistent home");
    }
    seed_git_config(&home, prepared.git.as_ref())?;
    let uid = command_output("id", &["-u"])?;
    let gid = command_output("id", &["-g"])?;
    let env_file = LaunchEnvFile::new(&launch)?;
    let create_args = create_args(&CreateSpec {
        record: &record,
        cwd,
        home: &home,
        output: &output,
        tag: &tag,
        uid: &uid,
        gid: &gid,
        mode,
        passthrough: &options.pi_args,
        resume: resume.as_deref(),
        launch: &launch,
        env_file: env_file.path(),
    });
    let container_id =
        docker_output_owned(&create_args).context("failed to create worker container")?;
    if container_id.is_empty() {
        bail!("docker create returned an empty container ID");
    }
    record.container_id = Some(container_id.clone());
    update_record(&output, &mut record)?;
    let terminal = supervise(&output, &container_id, mode, prompt, deadline, &mut record);
    let cleanup = docker_status(&["rm", "-f", &container_id]);
    let (status, reason, wrapper, raw, response) = match terminal {
        Ok(v) => v,
        Err(e) => (
            "failed".into(),
            format!("{e:#}"),
            3,
            inspect_exit(&container_id),
            None,
        ),
    };
    record.state = "terminal".into();
    let _ = update_record(&output, &mut record);
    if let Some(text) = response.as_deref() {
        atomic_write(&output.join("response.txt"), text.as_bytes())?;
    } else if mode == Mode::Exec {
        atomic_write(&output.join("response.txt"), b"")?;
    }
    let result = ResultRecord {
        schema_version: SCHEMA_VERSION,
        run_id,
        status: status.clone(),
        reason,
        wrapper_exit_code: wrapper,
        container_exit_code: raw,
        started_at_unix_ms: started,
        finished_at_unix_ms: now_ms(),
        response_available: response.is_some(),
        usage: None,
    };
    write_json_atomic(&output.join("result.json"), &result)?;
    if !cleanup {
        eprintln!("warning: failed to remove owned container {container_id}");
    }
    if wrapper != 0 {
        return Err(ExitError {
            code: wrapper,
            message: format!(
                "run ended with status {status}; see {}",
                output.join("result.json").display()
            ),
        }
        .into());
    }
    Ok(())
}

type Terminal = (String, String, i32, Option<i32>, Option<String>);

fn supervise(
    output: &Path,
    id: &str,
    mode: Mode,
    prompt: Option<Vec<u8>>,
    deadline: Option<Instant>,
    record: &mut RunRecord,
) -> Result<Terminal> {
    let mut child = Command::new("docker")
        .args(["start", "-a", "-i", id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to attach to worker container")?;
    record.state = "running".into();
    update_record(output, record)?;
    let mut child_in = child.stdin.take().context("docker stdin unavailable")?;
    let stdout = child.stdout.take().context("docker stdout unavailable")?;
    let stderr = child.stderr.take().context("docker stderr unavailable")?;
    let stderr_path = output.join("stderr.log");
    let stderr_thread = thread::spawn(move || -> io::Result<()> {
        let mut source = stderr;
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(stderr_path)?;
        let mut buf = [0u8; 8192];
        loop {
            let n = source.read(&mut buf)?;
            if n == 0 {
                break;
            }
            log.write_all(&buf[..n])?;
            io::stderr().write_all(&buf[..n])?;
            io::stderr().flush()?;
        }
        Ok(())
    });
    if mode == Mode::Exec {
        child_in
            .write_all(prompt.as_deref().unwrap_or_default())
            .context("failed to send prompt")?;
        child_in.flush()?;
        drop(child_in);
    } else {
        thread::spawn(move || {
            let _ = io::copy(&mut io::stdin().lock(), &mut child_in);
        });
    }
    let events_path = output.join("events.jsonl");
    let (tx, rx) = mpsc::sync_channel::<Result<Option<String>>>(32);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => {
                    let _ = tx.send(Ok(None));
                    break;
                }
                Ok(_) if line.len() > MAX_RECORD => {
                    let _ = tx.send(Err(anyhow::anyhow!("protocol record exceeds 32 MiB")));
                    break;
                }
                Ok(_) => match String::from_utf8(line.clone()) {
                    Ok(s) => {
                        if tx.send(Ok(Some(s))).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(Err(anyhow::anyhow!("protocol output is not UTF-8")));
                        break;
                    }
                },
                Err(e) => {
                    let _ = tx.send(Err(e.into()));
                    break;
                }
            }
        }
    });
    let mut seq = 0u64;
    let mut response = None;
    let mut protocol_error = None;
    let mut events = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&events_path)?;
    if mode == Mode::Exec {
        seq += 1;
        let envelope = json!({"schema_version":SCHEMA_VERSION,"type":"run_started","run_id":record.run_id,"seq":seq});
        write_json_line(&mut events, &envelope)?;
        write_json_line(&mut io::stdout(), &envelope)?;
    }
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            record.state = "stopping".into();
            let _ = update_record(output, record);
            stop_container(id, 10);
            let _ = child.wait();
            return Ok((
                "timed_out".into(),
                "timeout".into(),
                124,
                inspect_exit(id),
                response,
            ));
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(Some(line))) => {
                if mode == Mode::Rpc {
                    events.write_all(line.as_bytes())?;
                    io::stdout().write_all(line.as_bytes())?;
                } else {
                    let event: Value = match serde_json::from_str(line.trim_end()) {
                        Ok(v @ Value::Object(_)) => v,
                        _ => {
                            protocol_error = Some("invalid pi JSON event".to_string());
                            stop_container(id, 1);
                            break;
                        }
                    };
                    if let Some(text) = final_assistant_text(&event) {
                        response = Some(text);
                    }
                    seq += 1;
                    let envelope = json!({"schema_version":SCHEMA_VERSION,"type":"pi_event","run_id":record.run_id,"seq":seq,"event":event});
                    write_json_line(&mut events, &envelope)?;
                    write_json_line(&mut io::stdout(), &envelope)?;
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                protocol_error = Some(e.to_string());
                stop_container(id, 1);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child.try_wait()?.is_some() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait()?;
    let _ = stderr_thread.join();
    if let Some(error) = protocol_error {
        return Ok(("failed".into(), error, 1, status.code(), response));
    }
    if mode == Mode::Exec {
        seq += 1;
        let outcome = if status.success() {
            "completed"
        } else {
            "failed"
        };
        let envelope = json!({"schema_version":SCHEMA_VERSION,"type":"run_finished","run_id":record.run_id,"seq":seq,"status":outcome,"exit_code":status.code()});
        write_json_line(&mut events, &envelope)?;
        write_json_line(&mut io::stdout(), &envelope)?;
    }
    outcome(status, mode, response)
}

fn outcome(status: ExitStatus, mode: Mode, response: Option<String>) -> Result<Terminal> {
    if status.success() {
        Ok((
            "completed".into(),
            if mode == Mode::Rpc {
                "connection_closed"
            } else {
                "normal_exit"
            }
            .into(),
            0,
            status.code(),
            response,
        ))
    } else {
        Ok((
            "failed".into(),
            "worker_exit".into(),
            1,
            status.code(),
            response,
        ))
    }
}

struct CreateSpec<'a> {
    record: &'a RunRecord,
    cwd: &'a Path,
    home: &'a Path,
    output: &'a Path,
    tag: &'a str,
    uid: &'a str,
    gid: &'a str,
    mode: Mode,
    passthrough: &'a [String],
    resume: Option<&'a Path>,
    launch: &'a crate::ResolvedRunConfig,
    env_file: &'a Path,
}
fn create_args(spec: &CreateSpec<'_>) -> Vec<String> {
    let mut a = vec![
        "create".into(),
        "--init".into(),
        "-i".into(),
        "--label".into(),
        format!("io.pi-sandbox.run-id={}", spec.record.run_id),
        "--label".into(),
        "io.pi-sandbox.managed=true".into(),
    ];
    spec.launch.append_docker_create_args(
        &mut a,
        &ContainerContext {
            cwd: spec.cwd,
            home: spec.home,
            uid: spec.uid,
            gid: spec.gid,
            env_file: Some(spec.env_file),
        },
    );
    a.extend([
        "--mount".into(),
        format!(
            "type=bind,src={},dst=/tmp/pi-sessions",
            spec.output.join("sessions").display()
        ),
        spec.tag.into(),
        "pi".into(),
    ]);
    if spec.mode == Mode::Exec {
        a.extend(["--print".into(), "--mode".into(), "json".into()]);
    } else {
        a.extend(["--mode".into(), "rpc".into()]);
    }
    a.extend(["--session-dir".into(), "/tmp/pi-sessions".into()]);
    if let Some(source) = spec.resume
        && let Some(name) = source.file_name()
    {
        a.extend([
            "--session".into(),
            format!("/tmp/pi-sessions/{}", name.to_string_lossy()),
        ]);
    }
    a.extend(spec.passthrough.iter().cloned());
    a
}

fn validate_pi_args(args: &[String]) -> Result<()> {
    let values = [
        "--provider",
        "--model",
        "--thinking",
        "--tools",
        "--extension",
        "-e",
    ];
    let switches = ["--no-tools"];
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" || !arg.starts_with('-') || arg.starts_with('@') {
            bail!("unsupported positional pi argument `{arg}`");
        }
        if [
            "--mode",
            "--print",
            "-p",
            "--session",
            "--session-dir",
            "--continue",
            "-c",
            "--resume",
        ]
        .iter()
        .any(|x| arg == x || arg.starts_with(&format!("{x}=")))
        {
            bail!("pi option `{arg}` is managed by the launcher");
        }
        if switches.contains(&arg.as_str()) {
            i += 1;
            continue;
        }
        if values.iter().any(|x| arg == x) {
            if i + 1 >= args.len() {
                bail!("pi option `{arg}` requires a value");
            }
            i += 2;
            continue;
        }
        if values.iter().any(|x| arg.starts_with(&format!("{x}="))) {
            i += 1;
            continue;
        }
        bail!("unsupported pi option `{arg}`");
    }
    Ok(())
}

fn read_prompt(source: &Prompt) -> Result<Vec<u8>> {
    let bytes = match source {
        Prompt::File(path) => {
            let meta = fs::metadata(path)
                .with_context(|| format!("failed to stat prompt {}", path.display()))?;
            if meta.len() > MAX_PROMPT {
                bail!("prompt exceeds 8 MiB");
            }
            fs::read(path)?
        }
        Prompt::Stdin => {
            let mut b = Vec::new();
            io::stdin().take(MAX_PROMPT + 1).read_to_end(&mut b)?;
            b
        }
    };
    if bytes.len() as u64 > MAX_PROMPT {
        bail!("prompt exceeds 8 MiB");
    }
    if std::str::from_utf8(&bytes).is_err() {
        bail!("prompt is not valid UTF-8");
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        bail!("prompt must not be empty");
    }
    Ok(bytes)
}

fn prepare_resume_source(path: Option<&Path>) -> Result<Option<PathBuf>> {
    path.map(|p| {
        let p = fs::canonicalize(p)
            .with_context(|| format!("resume session does not exist: {}", p.display()))?;
        if !p.is_file() {
            bail!("resume session is not a file: {}", p.display());
        }
        Ok(p)
    })
    .transpose()
}

fn claim_output(path: &Path, cwd: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    if path.exists() {
        if !path.is_dir() || fs::read_dir(&path)?.next().is_some() {
            bail!(
                "output directory must be nonexistent or empty: {}",
                path.display()
            );
        }
    } else {
        fs::create_dir(&path)
            .with_context(|| format!("failed to claim output directory {}", path.display()))?;
    }
    set_private_permissions(&path)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path.join(".claim"))
        .context("output directory is already claimed")?;
    Ok(path)
}

fn lock_worktree(cwd: &Path) -> Result<File> {
    let canonical = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let path = std::env::temp_dir().join(format!(
        "pi-sandbox-worktree-{:016x}.lock",
        fnv1a64(canonical.to_string_lossy().as_bytes())
    ));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.try_lock_exclusive()
        .context("another headless run already owns this worktree")?;
    Ok(file)
}

fn stop(output_dir: &Path, grace: u64) -> Result<()> {
    let path = output_dir.join("run.json");
    let mut r: RunRecord = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
    )?;
    if r.state == "terminal" {
        return Ok(());
    }
    let id = r
        .container_id
        .clone()
        .context("run has no container ID yet")?;
    if let Some(expected) = r.docker_context.as_deref() {
        let actual = docker_output(&["context", "show"])?;
        if actual != expected {
            bail!("Docker context changed from `{expected}` to `{actual}`; refusing cleanup");
        }
    }
    let label = docker_output(&[
        "inspect",
        "--format",
        "{{ index .Config.Labels \"io.pi-sandbox.run-id\" }}",
        &id,
    ])?;
    if label != r.run_id.to_string() {
        bail!("container ownership label does not match run record");
    }
    r.cancellation_requested = true;
    r.state = "stopping".into();
    update_record(output_dir, &mut r)?;
    stop_container(&id, grace);
    Ok(())
}

fn stop_container(id: &str, grace: u64) {
    let _ = Command::new("docker")
        .args(["stop", "--time", &grace.to_string(), id])
        .status();
    let _ = Command::new("docker").args(["rm", "-f", id]).status();
}
fn inspect_exit(id: &str) -> Option<i32> {
    docker_output(&["inspect", "--format", "{{.State.ExitCode}}", id])
        .ok()?
        .parse()
        .ok()
}
fn final_assistant_text(v: &Value) -> Option<String> {
    let message = v.get("message")?;
    if message.get("role")?.as_str()? != "assistant" {
        return None;
    }
    if let Some(s) = message.get("content").and_then(Value::as_str) {
        return Some(s.into());
    }
    let text = message
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|p| {
            (p.get("type")?.as_str()? == "text")
                .then(|| p.get("text")?.as_str().map(str::to_owned))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("");
    Some(text)
}
fn write_json_line(w: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *w, value)?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}
fn update_record(output: &Path, record: &mut RunRecord) -> Result<()> {
    record.updated_at_unix_ms = now_ms();
    write_json_atomic(&output.join("run.json"), record)
}
fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    atomic_write(path, &data)
}
fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    {
        let mut f = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(tmp, path)?;
    Ok(())
}
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn docker_output(args: &[&str]) -> Result<String> {
    let o = Command::new("docker")
        .args(args)
        .output()
        .context("failed to run Docker")?;
    if !o.status.success() {
        bail!(
            "Docker command failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(String::from_utf8(o.stdout)?.trim().into())
}
fn docker_output_owned(args: &[String]) -> Result<String> {
    let o = Command::new("docker")
        .args(args)
        .output()
        .context("failed to run Docker")?;
    if !o.status.success() {
        bail!(
            "Docker create failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(String::from_utf8(o.stdout)?.trim().into())
}
fn docker_status(args: &[&str]) -> bool {
    Command::new("docker")
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}
#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_private_permissions(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_managed_and_unknown_pi_args() {
        assert!(validate_pi_args(&["--mode=json".into()]).is_err());
        assert!(validate_pi_args(&["--wat".into()]).is_err());
    }
    #[test]
    fn accepts_supported_value_forms() {
        validate_pi_args(&[
            "--provider".into(),
            "openai".into(),
            "--model=gpt-5".into(),
            "--no-tools".into(),
        ])
        .unwrap();
    }
    #[test]
    fn extracts_authoritative_message_text() {
        let v = json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}});
        assert_eq!(final_assistant_text(&v).as_deref(), Some("hello"));
    }
}
