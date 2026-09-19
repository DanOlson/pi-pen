//! Opt-in real-Docker coverage. Run with:
//! `cargo test --test docker_network -- --ignored --nocapture`

use pi_sandbox::launch::{EnvProvenance, ResolvedVariable};
use pi_sandbox::{
    ContainerContext, HostAccess, LaunchEnvFile, ResolvedRunConfig, RunOverrides, resolve_env,
};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

struct Fixtures {
    networks: Vec<String>,
    containers: Vec<String>,
}
impl Drop for Fixtures {
    fn drop(&mut self) {
        for name in &self.containers {
            let _ = Command::new("docker").args(["rm", "-f", name]).output();
        }
        for network in &self.networks {
            let _ = Command::new("docker")
                .args(["network", "rm", network])
                .output();
        }
    }
}

fn docker(args: &[String]) -> String {
    let output = Command::new("docker")
        .args(args)
        .output()
        .expect("run docker");
    assert!(
        output.status.success(),
        "docker {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .trim()
    .to_string()
}

fn variable(value: &str) -> ResolvedVariable {
    ResolvedVariable {
        value: value.into(),
        provenance: EnvProvenance::Cli,
    }
}

fn session_args(
    name: &str,
    networks: &[String],
    cwd: &Path,
    home: &Path,
    env: BTreeMap<String, ResolvedVariable>,
    command: &str,
) -> (LaunchEnvFile, Vec<String>, String) {
    let resolved = resolve_env(None, cwd).unwrap();
    let overrides = RunOverrides {
        networks: Some(networks.to_vec()),
        host_access: Some(HostAccess::Desktop),
        env: env
            .into_iter()
            .map(|(key, value)| (key, value.value))
            .collect(),
        ..Default::default()
    };
    let config = pi_sandbox::launch::resolve_run_config(&resolved, cwd, &overrides).unwrap();
    let file = LaunchEnvFile::new(&config).unwrap();
    let mut args = vec![
        "run".into(),
        "--detach".into(),
        "--name".into(),
        name.into(),
        "--label".into(),
        "pi-sandbox.test=networked-resources".into(),
    ];
    config.append_docker_create_args(
        &mut args,
        &ContainerContext {
            cwd,
            home,
            uid: "0",
            gid: "0",
            env_file: Some(file.path()),
        },
    );
    args.extend([
        "postgis/postgis:17-3.5".into(),
        "bash".into(),
        "-c".into(),
        command.into(),
    ]);
    let namespace = config.namespace.clone();
    (file, args, namespace)
}

#[test]
#[ignore = "requires a local Docker daemon and disposable Postgres/Redis images"]
fn concurrent_sessions_share_services_but_isolate_selected_resources() {
    let suffix = format!("{}", std::process::id());
    let pg_network = format!("pi-sandbox-pg-net-{suffix}");
    let redis_network = format!("pi-sandbox-redis-net-{suffix}");
    let postgres = format!("pi-sandbox-pg-{suffix}");
    let redis = format!("pi-sandbox-redis-{suffix}");
    let a = format!("pi-sandbox-a-{suffix}");
    let b = format!("pi-sandbox-b-{suffix}");
    let fixtures = Fixtures {
        networks: vec![pg_network.clone(), redis_network.clone()],
        containers: vec![a.clone(), b.clone(), postgres.clone(), redis.clone()],
    };
    docker(&[
        "network".into(),
        "create".into(),
        "--label".into(),
        "pi-sandbox.test=networked-resources".into(),
        pg_network.clone(),
    ]);
    docker(&[
        "network".into(),
        "create".into(),
        "--label".into(),
        "pi-sandbox.test=networked-resources".into(),
        redis_network.clone(),
    ]);
    docker(&[
        "run".into(),
        "--detach".into(),
        "--name".into(),
        postgres.clone(),
        "--network".into(),
        pg_network.clone(),
        "--env".into(),
        "POSTGRES_PASSWORD=test-only".into(),
        "--env".into(),
        "POSTGRES_DB=shared".into(),
        "postgis/postgis:17-3.5".into(),
    ]);
    docker(&[
        "run".into(),
        "--detach".into(),
        "--name".into(),
        redis.clone(),
        "--network".into(),
        redis_network.clone(),
        "redis:7-alpine".into(),
    ]);
    for _ in 0..60 {
        let pg = Command::new("docker")
            .args([
                "exec",
                &postgres,
                "pg_isready",
                "-h",
                "127.0.0.1",
                "-U",
                "postgres",
            ])
            .status()
            .unwrap()
            .success();
        let rd = Command::new("docker")
            .args(["exec", &redis, "redis-cli", "ping"])
            .status()
            .unwrap()
            .success();
        if pg && rd {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    for db in ["app_a", "app_b"] {
        docker(&[
            "exec".into(),
            postgres.clone(),
            "createdb".into(),
            "-U".into(),
            "postgres".into(),
            db.into(),
        ]);
    }
    let worktree_fixture = tempdir().unwrap();
    let repo = worktree_fixture.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    docker_command(Command::new("git").args(["init", "-q"]).current_dir(&repo));
    std::fs::write(repo.join("base"), "base\n").unwrap();
    docker_command(Command::new("git").args(["add", "base"]).current_dir(&repo));
    docker_command(
        Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "base",
            ])
            .current_dir(&repo),
    );
    let wt_a = worktree_fixture.path().join("worktree-a");
    let wt_b = worktree_fixture.path().join("worktree-b");
    docker_command(
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "network-a",
                wt_a.to_str().unwrap(),
            ])
            .current_dir(&repo),
    );
    docker_command(
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "network-b",
                wt_b.to_str().unwrap(),
            ])
            .current_dir(&repo),
    );
    let home_a = tempdir().unwrap();
    let home_b = tempdir().unwrap();
    let command = "psql -h \"$PGHOST\" -U postgres -d \"$PGDATABASE\" -c \"create table if not exists records(v text); insert into records values ('$PI_SANDBOX_NAMESPACE');\" && psql -h \"$PGHOST\" -U postgres -d shared -c \"create table if not exists records(v text); insert into records values ('$PI_SANDBOX_NAMESPACE');\" && exec 3<>/dev/tcp/$REDIS_HOST/6379 && key=\"${APP_REDIS_PREFIX}seen\" && printf '*3\\r\\n$3\\r\\nSET\\r\\n$%s\\r\\n%s\\r\\n$1\\r\\n1\\r\\n' \"${#key}\" \"$key\" >&3 && IFS= read -r reply <&3 && test \"$reply\" = \"$(printf '+OK\\r')\" && exec sleep 30";
    let common = |db: &str, prefix: &str| {
        BTreeMap::from([
            ("PGHOST".into(), variable(&postgres)),
            ("PGPASSWORD".into(), variable("test-only")),
            ("PGDATABASE".into(), variable(db)),
            ("REDIS_HOST".into(), variable(&redis)),
            ("APP_REDIS_PREFIX".into(), variable(prefix)),
        ])
    };
    let networks = vec![pg_network, redis_network];
    let (_file_a, args_a, namespace_a) = session_args(
        &a,
        &networks,
        &wt_a,
        home_a.path(),
        common("app_a", "a:"),
        command,
    );
    let (_file_b, args_b, namespace_b) = session_args(
        &b,
        &networks,
        &wt_b,
        home_b.path(),
        common("app_b", "b:"),
        command,
    );
    docker(&args_a);
    docker(&args_b);
    for _ in 0..60 {
        let ready = ["app_a", "app_b"].iter().all(|db| {
            Command::new("docker")
                .args([
                    "exec",
                    &postgres,
                    "psql",
                    "-U",
                    "postgres",
                    "-d",
                    db,
                    "-Atc",
                    "select to_regclass('records') is not null",
                ])
                .output()
                .is_ok_and(|output| {
                    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "t"
                })
        });
        if ready {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let running = docker(&[
        "ps".into(),
        "--filter".into(),
        "label=pi-sandbox.test=networked-resources".into(),
        "--format".into(),
        "{{.Names}}".into(),
    ]);
    if !running.contains(&a) || !running.contains(&b) {
        let states = docker(&[
            "ps".into(),
            "--all".into(),
            "--filter".into(),
            "label=pi-sandbox.test=networked-resources".into(),
            "--format".into(),
            "{{.Names}} {{.Status}}".into(),
        ]);
        let logs_a = docker(&["logs".into(), a.clone()]);
        let logs_b = docker(&["logs".into(), b.clone()]);
        panic!("sessions exited early:\n{states}\na: {logs_a}\nb: {logs_b}");
    }
    assert!(running.contains(&a) && running.contains(&b));
    docker(&["rm".into(), "-f".into(), a.clone()]);
    assert_eq!(
        docker(&[
            "inspect".into(),
            "-f".into(),
            "{{.State.Running}}".into(),
            b.clone()
        ]),
        "true"
    );
    assert_ne!(namespace_a, namespace_b);
    let relaunched = pi_sandbox::launch::resolve_run_config(
        &resolve_env(None, &wt_a).unwrap(),
        &wt_a,
        &RunOverrides::default(),
    )
    .unwrap();
    assert_eq!(relaunched.namespace, namespace_a);
    for (db, expected) in [("app_a", &namespace_a), ("app_b", &namespace_b)] {
        let count = docker(&[
            "exec".into(),
            postgres.clone(),
            "psql".into(),
            "-U".into(),
            "postgres".into(),
            "-d".into(),
            db.into(),
            "-Atc".into(),
            format!("select count(*) from records where v='{expected}'"),
        ]);
        assert_eq!(count, "1");
    }
    assert_eq!(
        docker(&[
            "exec".into(),
            redis.clone(),
            "redis-cli".into(),
            "exists".into(),
            "a:seen".into(),
            "b:seen".into()
        ]),
        "2"
    );
    assert_eq!(
        docker(&[
            "exec".into(),
            postgres.clone(),
            "psql".into(),
            "-U".into(),
            "postgres".into(),
            "-d".into(),
            "shared".into(),
            "-Atc".into(),
            "select count(*) from records".into()
        ]),
        "2"
    );
    drop(fixtures);
}

#[test]
#[ignore = "requires Docker Desktop host.docker.internal"]
fn docker_desktop_reaches_a_real_host_listener() {
    let listener = TcpListener::bind("0.0.0.0:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        use std::io::{Read, Write};
        let mut request = [0_u8; 256];
        let _ = stream.read(&mut request);
        stream
            .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 7\r\n\r\nhost-ok")
            .unwrap();
    });
    let dir = tempdir().unwrap();
    let config = ResolvedRunConfig {
        networks: Vec::new(),
        host_access: HostAccess::Desktop,
        namespace: "host_test".into(),
        worktree_id: "wt_host_test".into(),
        env: BTreeMap::new(),
        git: None,
    };
    let mut args = vec!["run".into(), "--rm".into()];
    config.append_docker_create_args(
        &mut args,
        &ContainerContext {
            cwd: dir.path(),
            home: dir.path(),
            uid: "0",
            gid: "0",
            env_file: None,
        },
    );
    args.extend(["postgis/postgis:17-3.5".into(), "bash".into(), "-c".into(), format!("exec 3<>/dev/tcp/host.docker.internal/{port}; printf 'GET / HTTP/1.0\\r\\n\\r\\n' >&3; cat <&3")]);
    assert!(docker(&args).contains("host-ok"));
    server.join().unwrap();
}

#[test]
#[ignore = "requires Docker Desktop and disposable published Postgres/Redis fixtures"]
fn docker_desktop_reaches_postgres_and_redis_through_host() {
    let suffix = std::process::id().to_string();
    let postgres = format!("pi-sandbox-host-pg-{suffix}");
    let redis = format!("pi-sandbox-host-redis-{suffix}");
    let fixtures = Fixtures {
        networks: Vec::new(),
        containers: vec![postgres.clone(), redis.clone()],
    };
    docker(&[
        "run".into(),
        "--detach".into(),
        "--name".into(),
        postgres.clone(),
        "--publish".into(),
        "127.0.0.1::5432".into(),
        "--env".into(),
        "POSTGRES_PASSWORD=test-only".into(),
        "postgis/postgis:17-3.5".into(),
    ]);
    docker(&[
        "run".into(),
        "--detach".into(),
        "--name".into(),
        redis.clone(),
        "--publish".into(),
        "127.0.0.1::6379".into(),
        "redis:7-alpine".into(),
    ]);
    for _ in 0..60 {
        let pg = Command::new("docker")
            .args([
                "exec",
                &postgres,
                "pg_isready",
                "-h",
                "127.0.0.1",
                "-U",
                "postgres",
            ])
            .status()
            .unwrap()
            .success();
        let rd = Command::new("docker")
            .args(["exec", &redis, "redis-cli", "ping"])
            .status()
            .unwrap()
            .success();
        if pg && rd {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let pg_port = docker(&["port".into(), postgres.clone(), "5432/tcp".into()])
        .rsplit(':')
        .next()
        .unwrap()
        .to_string();
    let redis_port = docker(&["port".into(), redis.clone(), "6379/tcp".into()])
        .rsplit(':')
        .next()
        .unwrap()
        .to_string();
    let dir = tempdir().unwrap();
    let env = BTreeMap::from([
        ("PGPASSWORD".into(), variable("test-only")),
        ("PGPORT".into(), variable(&pg_port)),
        ("REDIS_PORT".into(), variable(&redis_port)),
    ]);
    let config = ResolvedRunConfig {
        networks: Vec::new(),
        host_access: HostAccess::Desktop,
        namespace: "host_services".into(),
        worktree_id: "wt_host_services".into(),
        env,
        git: None,
    };
    let file = LaunchEnvFile::new(&config).unwrap();
    let mut args = vec!["run".into(), "--rm".into()];
    config.append_docker_create_args(
        &mut args,
        &ContainerContext {
            cwd: dir.path(),
            home: dir.path(),
            uid: "0",
            gid: "0",
            env_file: Some(file.path()),
        },
    );
    args.extend(["postgis/postgis:17-3.5".into(), "bash".into(), "-c".into(), "psql -h host.docker.internal -p \"$PGPORT\" -U postgres -Atc 'select 42' | grep -qx 42 && exec 3<>/dev/tcp/host.docker.internal/$REDIS_PORT && printf '*1\\r\\n$4\\r\\nPING\\r\\n' >&3 && IFS= read -r reply <&3 && test \"$reply\" = \"$(printf '+PONG\\r')\"".into()]);
    docker(&args);
    drop(fixtures);
}

#[test]
#[ignore = "requires Docker and Git linked-worktree support"]
fn two_linked_worktrees_run_git_operations_concurrently() {
    let repo = tempdir().unwrap();
    docker_command(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path()),
    );
    std::fs::write(repo.path().join("base"), "base\n").unwrap();
    docker_command(
        Command::new("git")
            .args(["add", "base"])
            .current_dir(repo.path()),
    );
    docker_command(
        Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "base",
            ])
            .current_dir(repo.path()),
    );
    let parent = repo.path().parent().unwrap();
    let a_path = parent.join(format!("pi-wt-a-{}", std::process::id()));
    let b_path = parent.join(format!("pi-wt-b-{}", std::process::id()));
    docker_command(
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "test-a",
                a_path.to_str().unwrap(),
            ])
            .current_dir(repo.path()),
    );
    docker_command(
        Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "-b",
                "test-b",
                b_path.to_str().unwrap(),
            ])
            .current_dir(repo.path()),
    );
    let pointer_a = std::fs::read(a_path.join(".git")).unwrap();
    let pointer_b = std::fs::read(b_path.join(".git")).unwrap();
    let names = [
        format!("pi-sandbox-git-a-{}", std::process::id()),
        format!("pi-sandbox-git-b-{}", std::process::id()),
    ];
    let images = docker(&[
        "image".into(),
        "ls".into(),
        "pi-sandbox-env:*".into(),
        "--format".into(),
        "{{.Repository}}:{{.Tag}}".into(),
    ]);
    let image = images
        .lines()
        .find(|image| {
            Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    image,
                    "sh",
                    "-c",
                    "command -v git >/dev/null",
                ])
                .status()
                .is_ok_and(|status| status.success())
        })
        .expect("build a pi-sandbox environment image containing Git before this test")
        .to_string();
    let fixtures = Fixtures {
        networks: Vec::new(),
        containers: names.to_vec(),
    };
    for (index, path) in [&a_path, &b_path].into_iter().enumerate() {
        let resolved = resolve_env(None, path).unwrap();
        let config =
            pi_sandbox::launch::resolve_run_config(&resolved, path, &RunOverrides::default())
                .unwrap();
        let file = LaunchEnvFile::new(&config).unwrap();
        let home = tempdir().unwrap().keep();
        let uid = docker_output(Command::new("id").arg("-u"));
        let gid = docker_output(Command::new("id").arg("-g"));
        let mut args = vec![
            "run".into(),
            "--detach".into(),
            "--name".into(),
            names[index].clone(),
        ];
        config.append_docker_create_args(
            &mut args,
            &ContainerContext {
                cwd: path,
                home: &home,
                uid: &uid,
                gid: &gid,
                env_file: Some(file.path()),
            },
        );
        args.extend([image.clone(), "sh".into(), "-c".into(), format!("set -ex; git config --global --add safe.directory /workspace && git status --porcelain && echo {index} > change-{index} && git add change-{index} && git -c user.name=Test -c user.email=test@example.invalid commit -qm change-{index} && touch done-{index} && exec sleep 30")]);
        docker(&args);
    }
    for _ in 0..100 {
        if a_path.join("done-0").exists() && b_path.join("done-1").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if !a_path.join("done-0").exists() || !b_path.join("done-1").exists() {
        panic!(
            "linked worktree sessions failed:\na: {}\nb: {}",
            docker(&["logs".into(), names[0].clone()]),
            docker(&["logs".into(), names[1].clone()])
        );
    }
    assert_eq!(std::fs::read(a_path.join(".git")).unwrap(), pointer_a);
    assert_eq!(std::fs::read(b_path.join(".git")).unwrap(), pointer_b);
    assert_eq!(
        docker_output(
            Command::new("git")
                .args(["log", "-1", "--format=%s"])
                .current_dir(&a_path)
        ),
        "change-0"
    );
    assert_eq!(
        docker_output(
            Command::new("git")
                .args(["log", "-1", "--format=%s"])
                .current_dir(&b_path)
        ),
        "change-1"
    );
    drop(fixtures);
    docker_command(
        Command::new("git")
            .args(["worktree", "remove", "--force", a_path.to_str().unwrap()])
            .current_dir(repo.path()),
    );
    docker_command(
        Command::new("git")
            .args(["worktree", "remove", "--force", b_path.to_str().unwrap()])
            .current_dir(repo.path()),
    );
}

fn docker_command(command: &mut Command) {
    let status = command.status().unwrap();
    assert!(status.success());
}
fn docker_output(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().into()
}
