//! Opt-in real-Docker coverage. Run with:
//! `cargo test --test docker_network -- --ignored --nocapture`

use pi_sandbox::launch::{EnvProvenance, ResolvedVariable};
use pi_sandbox::{ContainerContext, HostAccess, LaunchEnvFile, ResolvedRunConfig};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

struct Fixtures {
    network: String,
    containers: Vec<String>,
}
impl Drop for Fixtures {
    fn drop(&mut self) {
        for name in &self.containers {
            let _ = Command::new("docker").args(["rm", "-f", name]).output();
        }
        let _ = Command::new("docker")
            .args(["network", "rm", &self.network])
            .output();
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
    network: &str,
    home: &Path,
    mut env: BTreeMap<String, ResolvedVariable>,
    command: &str,
) -> (LaunchEnvFile, Vec<String>) {
    env.insert(
        "PI_SANDBOX_NAMESPACE".into(),
        ResolvedVariable {
            value: name.into(),
            provenance: EnvProvenance::Injected,
        },
    );
    let config = ResolvedRunConfig {
        networks: vec![network.into()],
        host_access: HostAccess::Desktop,
        namespace: name.into(),
        worktree_id: format!("wt_{name}"),
        env,
        git: None,
    };
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
            cwd: home,
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
    (file, args)
}

#[test]
#[ignore = "requires a local Docker daemon and disposable Postgres/Redis images"]
fn concurrent_sessions_share_services_but_isolate_selected_resources() {
    let suffix = format!("{}", std::process::id());
    let network = format!("pi-sandbox-it-{suffix}");
    let postgres = format!("pi-sandbox-pg-{suffix}");
    let redis = format!("pi-sandbox-redis-{suffix}");
    let a = format!("pi-sandbox-a-{suffix}");
    let b = format!("pi-sandbox-b-{suffix}");
    let fixtures = Fixtures {
        network: network.clone(),
        containers: vec![a.clone(), b.clone(), postgres.clone(), redis.clone()],
    };
    docker(&[
        "network".into(),
        "create".into(),
        "--label".into(),
        "pi-sandbox.test=networked-resources".into(),
        network.clone(),
    ]);
    docker(&[
        "run".into(),
        "--detach".into(),
        "--name".into(),
        postgres.clone(),
        "--network".into(),
        network.clone(),
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
        network.clone(),
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
    let dir = tempdir().unwrap();
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
    let (_file_a, args_a) = session_args(&a, &network, dir.path(), common("app_a", "a:"), command);
    let (_file_b, args_b) = session_args(&b, &network, dir.path(), common("app_b", "b:"), command);
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
    for (db, expected) in [("app_a", &a), ("app_b", &b)] {
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
