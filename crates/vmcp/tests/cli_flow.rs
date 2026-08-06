//! End-to-end CLI flows through the `vmcp` binary.
//!
//! Covers: init → add mcp/tool/skill/tasks → list/get → remove,
//! probe codegen, --no-spec, aliases, and failure paths.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use vmcp_registry::{load_registry_raw, load_sidecar};

fn tmp_dir(stem: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("vmcp-cli-flow-{stem}-{nanos}"));
    fs::create_dir_all(&p).unwrap();
    p
}

/// Absolute-path vmcp.toml so tests do not depend on process CWD.
fn abs_config(dir: &Path) -> PathBuf {
    let cfg = dir.join("vmcp.toml");
    let text = format!(
        r#"
host = "127.0.0.1"
port = 8765
mcp_path = "/mcp"
public_base_url = "http://localhost:8765"
registry_path = "{reg}"
lock_path     = "{lock}"
spec_dir      = "{specs}"
skills_dir    = "{skills}"
notif_ring_max = 10000

[gql]
max_depth = 10
max_complexity = 1000

[upstream]
spawn_timeout_ms = 30000
call_timeout_ms  = 10000

[auth]
enabled = false

[recorder]
sessions_dir = "{sessions}"
"#,
        reg = dir.join("registry.json").display(),
        lock = dir.join("tools.lock.json").display(),
        specs = dir.join("specs").display(),
        skills = dir.join("skills").display(),
        sessions = dir.join("sessions").display(),
    );
    fs::write(&cfg, text).unwrap();
    cfg
}

fn vmcp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vmcp"))
}

fn mock_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mock_delay_upstream"))
}

fn run(cfg: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(vmcp_bin());
    cmd.arg("--config").arg(cfg).args(args);
    cmd.output()
        .unwrap_or_else(|e| panic!("spawn vmcp {:?}: {e}", args))
}

fn assert_ok(out: &Output, label: &str) {
    if !out.status.success() {
        panic!(
            "{label} failed ({})\nstdout:\n{}\nstderr:\n{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
}

fn assert_err(out: &Output, label: &str, needle: &str) {
    assert!(
        !out.status.success(),
        "{label} expected failure, got success\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains(needle),
        "{label}: expected stderr/stdout to contain `{needle}`, got:\n{combined}"
    );
}

fn scaffold(dir: &Path) -> PathBuf {
    let status = Command::new(vmcp_bin())
        .args(["init", "--dir", dir.to_str().unwrap(), "--force"])
        .status()
        .expect("init");
    assert!(status.success(), "vmcp init failed");
    abs_config(dir)
}

/// Full operator flow: init → probe mcp → tool upsert → skill → tasks → list/get → remove.
#[test]
fn flow_init_probe_tool_skill_tasks_list_get_remove() {
    let dir = tmp_dir("full");
    let cfg = scaffold(&dir);
    let mock = mock_bin();

    // 1) add mcp with live tools/list → sidecar
    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--transport",
                "stdio",
                "--description",
                "mock delay",
                "mock",
                "--",
                mock.to_str().unwrap(),
            ],
        ),
        "add mcp probe",
    );

    let reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    assert_eq!(reg.upstreams.len(), 1);
    assert_eq!(reg.upstreams[0].name, "mock");
    assert_eq!(
        reg.upstreams[0].sidecar_spec.as_deref(),
        Some(Path::new("mock.json"))
    );
    let sc = load_sidecar(Some(&dir.join("specs/mock.json")))
        .unwrap()
        .expect("sidecar");
    let names: Vec<_> = sc.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"delay_read"), "{names:?}");
    assert!(names.contains(&"delay_write"), "{names:?}");

    // 2) upsert tool override (task_support)
    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "tool",
                "mock",
                "delay_read",
                "--read-only",
                "--task-support",
                "optional",
            ],
        ),
        "add tool",
    );
    let sc = load_sidecar(Some(&dir.join("specs/mock.json")))
        .unwrap()
        .unwrap();
    let t = sc.tools.iter().find(|t| t.name == "delay_read").unwrap();
    assert!(t.read_only);
    assert_eq!(
        t.task_support,
        Some(vmcp_registry::TaskSupportHint::Optional)
    );

    // 3) skill + tasks
    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "skill",
                "delay_playbook",
                "--description",
                "run delay",
                "--template",
                "Call run_task for delay_read",
            ],
        ),
        "add skill",
    );
    assert!(dir.join("skills/delay_playbook.yaml").exists());
    assert_ok(&run(&cfg, &["add", "tasks"]), "add tasks");
    let toml = fs::read_to_string(&cfg).unwrap();
    assert!(toml.lines().any(|l| l.trim() == "[tasks]"));
    assert!(toml.lines().any(|l| l.trim() == "enabled = true"));

    // 4) list / get
    let list_mcp = run(&cfg, &["list", "mcp"]);
    assert_ok(&list_mcp, "list mcp");
    assert!(String::from_utf8_lossy(&list_mcp.stdout).contains("mock"));

    let list_tool = run(&cfg, &["list", "tool", "mock"]);
    assert_ok(&list_tool, "list tool");
    let tool_out = String::from_utf8_lossy(&list_tool.stdout);
    assert!(tool_out.contains("delay_read"), "{tool_out}");
    assert!(tool_out.contains("task=optional"), "{tool_out}");

    let list_skill = run(&cfg, &["list", "skill"]);
    assert_ok(&list_skill, "list skill");
    assert!(String::from_utf8_lossy(&list_skill.stdout).contains("delay_playbook"));

    let get_mcp = run(&cfg, &["get", "mcp", "mock"]);
    assert_ok(&get_mcp, "get mcp");
    assert!(String::from_utf8_lossy(&get_mcp.stdout).contains("mock_delay"));

    let get_tool = run(&cfg, &["get", "tool", "mock", "delay_read"]);
    assert_ok(&get_tool, "get tool");
    assert!(String::from_utf8_lossy(&get_tool.stdout).contains("optional"));

    let get_skill = run(&cfg, &["get", "skill", "delay_playbook"]);
    assert_ok(&get_skill, "get skill");
    assert!(String::from_utf8_lossy(&get_skill.stdout).contains("run_task"));

    // 5) remove tool / skill / mcp
    assert_ok(
        &run(&cfg, &["remove", "tool", "mock", "delay_read"]),
        "remove tool",
    );
    let sc = load_sidecar(Some(&dir.join("specs/mock.json")))
        .unwrap()
        .unwrap();
    assert!(!sc.tools.iter().any(|t| t.name == "delay_read"));

    assert_ok(
        &run(&cfg, &["remove", "skill", "delay_playbook"]),
        "remove skill",
    );
    assert!(!dir.join("skills/delay_playbook.yaml").exists());

    assert_ok(&run(&cfg, &["remove", "mcp", "mock"]), "remove mcp");
    let reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    assert!(reg.upstreams.is_empty());

    let _ = fs::remove_dir_all(&dir);
}

/// `add mcp --no-spec` skips probe; later `add tool` links sidecar_spec.
#[test]
fn flow_no_spec_then_manual_tool_links_sidecar() {
    let dir = tmp_dir("nospec");
    let cfg = scaffold(&dir);

    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "http",
                "notion",
                "https://mcp.notion.com/mcp",
            ],
        ),
        "add mcp --no-spec",
    );
    let reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    assert!(reg.upstreams[0].sidecar_spec.is_none());
    assert!(!dir.join("specs/notion.json").exists());

    assert_ok(
        &run(&cfg, &["add", "tool", "notion", "search", "--read-only"]),
        "add tool after no-spec",
    );
    let reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    assert_eq!(
        reg.upstreams[0].sidecar_spec.as_deref(),
        Some(Path::new("notion.json"))
    );
    let sc = load_sidecar(Some(&dir.join("specs/notion.json")))
        .unwrap()
        .unwrap();
    assert_eq!(sc.tools.len(), 1);
    assert_eq!(sc.tools[0].name, "search");
    assert!(sc.tools[0].read_only);

    let _ = fs::remove_dir_all(&dir);
}

/// HTTP add with bearer placeholder must not expand on disk; `--env` rejected.
#[test]
fn flow_http_bearer_placeholder_and_env_rejected() {
    let dir = tmp_dir("http");
    let cfg = scaffold(&dir);

    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "http",
                "--bearer",
                "${API_KEY}",
                "secure",
                "https://api.example.com/mcp",
            ],
        ),
        "add http bearer",
    );
    let reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    assert_eq!(reg.upstreams[0].bearer.as_deref(), Some("${API_KEY}"));
    assert_eq!(
        reg.upstreams[0].url.as_deref(),
        Some("https://api.example.com/mcp")
    );

    assert_err(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "http",
                "--env",
                "K=V",
                "x",
                "https://x/mcp",
            ],
        ),
        "http+env",
        "--env is only valid",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Duplicate mcp name / unknown tool server / missing get targets.
#[test]
fn flow_errors_duplicate_unknown_missing() {
    let dir = tmp_dir("err");
    let cfg = scaffold(&dir);

    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "stdio",
                "time",
                "--",
                "true",
            ],
        ),
        "add time",
    );
    assert_err(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "stdio",
                "time",
                "--",
                "true",
            ],
        ),
        "duplicate",
        "duplicate",
    );

    assert_err(
        &run(&cfg, &["add", "tool", "missing", "foo", "--read-only"]),
        "tool without mcp",
        "unknown upstream",
    );

    assert_err(
        &run(&cfg, &["get", "mcp", "nope"]),
        "get missing mcp",
        "unknown upstream",
    );
    assert_err(
        &run(&cfg, &["remove", "skill", "nope"]),
        "remove missing skill",
        "No such file",
    );

    // stdio without command
    assert_err(
        &run(
            &cfg,
            &["add", "mcp", "--no-spec", "--transport", "stdio", "x"],
        ),
        "stdio no command",
        "requires a command",
    );

    // http without url
    assert_err(
        &run(
            &cfg,
            &["add", "mcp", "--no-spec", "--transport", "http", "y"],
        ),
        "http no url",
        "requires a URL",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// `vmcp mcp add|list|get|remove` aliases `add|list|get|remove mcp`.
#[test]
fn flow_mcp_alias_matches_add_mcp() {
    let dir = tmp_dir("alias");
    let cfg = scaffold(&dir);

    assert_ok(
        &run(
            &cfg,
            &[
                "mcp",
                "add",
                "--no-spec",
                "--transport",
                "http",
                "alias",
                "https://example.com/mcp",
            ],
        ),
        "mcp add alias",
    );
    let list = run(&cfg, &["mcp", "list"]);
    assert_ok(&list, "mcp list");
    assert!(String::from_utf8_lossy(&list.stdout).contains("alias"));

    let get = run(&cfg, &["mcp", "get", "alias"]);
    assert_ok(&get, "mcp get");
    assert!(String::from_utf8_lossy(&get.stdout).contains("example.com"));

    assert_ok(&run(&cfg, &["mcp", "remove", "alias"]), "mcp remove");
    let reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    assert!(reg.upstreams.is_empty());

    let _ = fs::remove_dir_all(&dir);
}

/// init refuses overwrite without --force; --force rewrites.
#[test]
fn flow_init_force_and_dirs() {
    let dir = tmp_dir("init");
    let status = Command::new(vmcp_bin())
        .args(["init", "--dir", dir.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
    for sub in [
        "specs",
        "skills",
        "state",
        "sessions",
        "registry.json",
        "vmcp.toml",
    ] {
        assert!(dir.join(sub).exists(), "missing {sub}");
    }

    let fail = Command::new(vmcp_bin())
        .args(["init", "--dir", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert_err(&fail, "init no force", "--force");

    let ok = Command::new(vmcp_bin())
        .args(["init", "--dir", dir.to_str().unwrap(), "--force"])
        .status()
        .unwrap();
    assert!(ok.success());

    let _ = fs::remove_dir_all(&dir);
}

/// Probe failure (bad command) must not leave a registry entry.
#[test]
fn flow_probe_failure_does_not_write_registry() {
    let dir = tmp_dir("probefail");
    let cfg = scaffold(&dir);

    let out = run(
        &cfg,
        &[
            "add",
            "mcp",
            "--transport",
            "stdio",
            "broken",
            "--",
            "/nonexistent/mcp-server-binary-xyz",
        ],
    );
    assert!(
        !out.status.success(),
        "probe should fail for missing binary"
    );

    let reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    assert!(
        reg.upstreams.is_empty(),
        "failed probe must not write registry, got {:?}",
        reg.upstreams
    );
    assert!(!dir.join("specs/broken.json").exists());

    let _ = fs::remove_dir_all(&dir);
}

/// Second `add tasks` flips enabled=false → true without duplicating the table.
#[test]
fn flow_add_tasks_idempotent_enable() {
    let dir = tmp_dir("tasks");
    let cfg = scaffold(&dir);

    // Pre-seed a disabled tasks table
    let mut toml = fs::read_to_string(&cfg).unwrap();
    toml.push_str("\n[tasks]\nenabled = false\ndb_path = \"state/tasks.db\"\n");
    fs::write(&cfg, &toml).unwrap();

    assert_ok(&run(&cfg, &["add", "tasks"]), "enable tasks");
    let text = fs::read_to_string(&cfg).unwrap();
    assert_eq!(
        text.matches("[tasks]").count(),
        1,
        "must not duplicate [tasks]"
    );
    // Inspect only the [tasks] table (auth also has enabled=false).
    let tasks = text.split("[tasks]").nth(1).expect("[tasks] present");
    let tasks_body = tasks.split("\n[").next().unwrap_or(tasks);
    assert!(
        tasks_body.lines().any(|l| l.trim() == "enabled = true"),
        "tasks section:\n{tasks_body}"
    );
    assert!(
        !tasks_body.lines().any(|l| l.trim() == "enabled = false"),
        "tasks section still disabled:\n{tasks_body}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Skill import via --file.
#[test]
fn flow_add_skill_from_file() {
    let dir = tmp_dir("skillfile");
    let cfg = scaffold(&dir);
    let src = dir.join("import.yaml");
    fs::write(
        &src,
        r#"
name: from_file
description: imported
template: |
  Hello {{name}}
"#,
    )
    .unwrap();

    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "skill",
                "from_file",
                "--description",
                "ignored-if-present",
                "--file",
                src.to_str().unwrap(),
            ],
        ),
        "add skill --file",
    );
    let yaml = fs::read_to_string(dir.join("skills/from_file.yaml")).unwrap();
    assert!(yaml.contains("imported"));
    assert!(yaml.contains("Hello"));

    let _ = fs::remove_dir_all(&dir);
}

/// Empty list outputs (mcp / tool / skill) succeed with stderr hints.
#[test]
fn flow_list_empty_states() {
    let dir = tmp_dir("empty");
    let cfg = scaffold(&dir);

    let list_mcp = run(&cfg, &["list", "mcp"]);
    assert_ok(&list_mcp, "list mcp empty");
    assert!(
        String::from_utf8_lossy(&list_mcp.stderr).contains("no upstreams"),
        "stderr={}",
        String::from_utf8_lossy(&list_mcp.stderr)
    );

    let list_tool = run(&cfg, &["list", "tool"]);
    assert_ok(&list_tool, "list tool empty");
    assert!(
        String::from_utf8_lossy(&list_tool.stderr).contains("no sidecar"),
        "stderr={}",
        String::from_utf8_lossy(&list_tool.stderr)
    );

    let list_skill = run(&cfg, &["list", "skill"]);
    assert_ok(&list_skill, "list skill empty");
    assert!(
        String::from_utf8_lossy(&list_skill.stderr).contains("no skills"),
        "stderr={}",
        String::from_utf8_lossy(&list_skill.stderr)
    );

    let _ = fs::remove_dir_all(&dir);
}

/// `list tool` without server aggregates across multiple sidecars.
#[test]
fn flow_list_tool_all_servers() {
    let dir = tmp_dir("listall");
    let cfg = scaffold(&dir);

    for (name, tool) in [("alpha", "a_tool"), ("beta", "b_tool")] {
        assert_ok(
            &run(
                &cfg,
                &[
                    "add",
                    "mcp",
                    "--no-spec",
                    "--transport",
                    "http",
                    name,
                    &format!("https://example.com/{name}"),
                ],
            ),
            &format!("add {name}"),
        );
        assert_ok(
            &run(&cfg, &["add", "tool", name, tool, "--read-only"]),
            &format!("add tool {name}.{tool}"),
        );
    }

    let out = run(&cfg, &["list", "tool"]);
    assert_ok(&out, "list tool all");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("alpha.a_tool"), "{stdout}");
    assert!(stdout.contains("beta.b_tool"), "{stdout}");

    let filtered = run(&cfg, &["list", "tool", "alpha"]);
    assert_ok(&filtered, "list tool alpha");
    let f = String::from_utf8_lossy(&filtered.stdout);
    assert!(f.contains("alpha.a_tool"), "{f}");
    assert!(!f.contains("beta"), "{f}");

    let _ = fs::remove_dir_all(&dir);
}

/// Tool upsert overwrites flags; get/remove missing tool paths.
#[test]
fn flow_tool_upsert_and_missing_errors() {
    let dir = tmp_dir("toolupsert");
    let cfg = scaffold(&dir);

    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "http",
                "srv",
                "https://example.com/mcp",
            ],
        ),
        "add mcp",
    );

    assert_err(
        &run(&cfg, &["get", "tool", "srv", "missing"]),
        "get before sidecar",
        "no sidecar",
    );
    assert_err(
        &run(&cfg, &["remove", "tool", "srv", "missing"]),
        "remove before sidecar",
        "no sidecar",
    );

    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "tool",
                "srv",
                "do_work",
                "--task-support",
                "required",
                "--description",
                "first",
            ],
        ),
        "add tool write",
    );
    let sc = load_sidecar(Some(&dir.join("specs/srv.json")))
        .unwrap()
        .unwrap();
    let t = sc.tools.iter().find(|t| t.name == "do_work").unwrap();
    assert!(!t.read_only);
    assert_eq!(
        t.task_support,
        Some(vmcp_registry::TaskSupportHint::Required)
    );
    assert_eq!(t.description.as_deref(), Some("first"));

    // Upsert flips to read_only + forbidden + new description
    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "tool",
                "srv",
                "do_work",
                "--read-only",
                "--task-support",
                "forbidden",
                "--description",
                "second",
            ],
        ),
        "upsert tool",
    );
    let sc = load_sidecar(Some(&dir.join("specs/srv.json")))
        .unwrap()
        .unwrap();
    assert_eq!(sc.tools.len(), 1, "upsert must not duplicate");
    let t = &sc.tools[0];
    assert!(t.read_only);
    assert_eq!(
        t.task_support,
        Some(vmcp_registry::TaskSupportHint::Forbidden)
    );
    assert_eq!(t.description.as_deref(), Some("second"));

    assert_err(
        &run(&cfg, &["get", "tool", "srv", "nope"]),
        "get unknown tool",
        "unknown tool",
    );
    assert_err(
        &run(&cfg, &["remove", "tool", "srv", "nope"]),
        "remove unknown tool",
        "unknown",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// stdio `--env` / `--cwd` persist; transport flag mismatches rejected.
#[test]
fn flow_stdio_env_cwd_and_flag_mismatches() {
    let dir = tmp_dir("stdioenv");
    let cfg = scaffold(&dir);
    let cwd = dir.join("workdir");
    fs::create_dir_all(&cwd).unwrap();

    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "stdio",
                "--env",
                "FOO=bar",
                "--env",
                "BAZ=qux",
                "--cwd",
                cwd.to_str().unwrap(),
                "--description",
                "with env",
                "envy",
                "--",
                "true",
            ],
        ),
        "add stdio env",
    );
    let reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    let u = &reg.upstreams[0];
    assert_eq!(u.env.get("FOO").map(String::as_str), Some("bar"));
    assert_eq!(u.env.get("BAZ").map(String::as_str), Some("qux"));
    assert_eq!(u.cwd.as_deref(), Some(cwd.as_path()));
    assert_eq!(u.description.as_deref(), Some("with env"));

    assert_err(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "stdio",
                "--bearer",
                "tok",
                "x",
                "--",
                "true",
            ],
        ),
        "stdio+bearer",
        "--bearer is only valid",
    );
    assert_err(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "http",
                "--cwd",
                cwd.to_str().unwrap(),
                "y",
                "https://example.com/mcp",
            ],
        ),
        "http+cwd",
        "--cwd is only valid",
    );
    assert_err(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "http",
                "z",
                "https://a/mcp",
                "https://b/mcp",
            ],
        ),
        "http extra url",
        "single URL",
    );
    assert_err(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--no-spec",
                "--transport",
                "stdio",
                "--env",
                "NOEQUALS",
                "bad",
                "--",
                "true",
            ],
        ),
        "bad env",
        "KEY=VALUE",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Skill validation: missing template, YAML name mismatch, unknown get.
#[test]
fn flow_skill_validation_errors() {
    let dir = tmp_dir("skillerr");
    let cfg = scaffold(&dir);

    assert_err(
        &run(
            &cfg,
            &["add", "skill", "bare", "--description", "no template"],
        ),
        "skill no template",
        "--template",
    );

    let src = dir.join("mismatch.yaml");
    fs::write(&src, "name: other\ndescription: x\ntemplate: hi\n").unwrap();
    assert_err(
        &run(
            &cfg,
            &[
                "add",
                "skill",
                "wanted",
                "--description",
                "x",
                "--file",
                src.to_str().unwrap(),
            ],
        ),
        "yaml name mismatch",
        "does not match",
    );

    assert_err(
        &run(&cfg, &["get", "skill", "ghost"]),
        "get missing skill",
        "unknown skill",
    );

    // Empty YAML name/description → filled from CLI args
    let anon = dir.join("anon.yaml");
    fs::write(&anon, "name: \"\"\ndescription: \"\"\ntemplate: body\n").unwrap();
    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "skill",
                "anon",
                "--description",
                "from-cli",
                "--file",
                anon.to_str().unwrap(),
            ],
        ),
        "skill anon file",
    );
    let yaml = fs::read_to_string(dir.join("skills/anon.yaml")).unwrap();
    assert!(
        yaml.contains("name: anon") || yaml.contains("name: \"anon\""),
        "{yaml}"
    );
    assert!(yaml.contains("from-cli"), "{yaml}");
    assert!(yaml.contains("body"), "{yaml}");

    let _ = fs::remove_dir_all(&dir);
}

/// Probe refuses missing `${VAR}` without writing registry; remove unknown mcp.
#[test]
fn flow_probe_missing_env_and_remove_unknown() {
    let dir = tmp_dir("probeenv");
    let cfg = scaffold(&dir);

    let out = run(
        &cfg,
        &[
            "add",
            "mcp",
            "--transport",
            "stdio",
            "--env",
            "TOKEN=${MISSING_VMCP_PROBE_VAR_XYZ}",
            "needs_env",
            "--",
            "true",
        ],
    );
    assert!(
        !out.status.success(),
        "probe expand should fail without MISSING_VMCP_PROBE_VAR_XYZ"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("expand") || combined.contains("MISSING") || combined.contains("env"),
        "unexpected error:\n{combined}"
    );
    let reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    assert!(reg.upstreams.is_empty(), "must not write on expand failure");

    assert_err(
        &run(&cfg, &["remove", "mcp", "ghost"]),
        "remove unknown mcp",
        "unknown",
    );

    let _ = fs::remove_dir_all(&dir);
}

/// `add tasks` ignores commented `# [tasks]` and fills missing `enabled`.
#[test]
fn flow_add_tasks_ignores_comment_and_fills_enabled() {
    let dir = tmp_dir("tasks2");
    let cfg = scaffold(&dir);

    let mut toml = fs::read_to_string(&cfg).unwrap();
    toml.push_str("\n# [tasks]\n# enabled = false\n\n[tasks]\ndb_path = \"state/tasks.db\"\n");
    fs::write(&cfg, &toml).unwrap();

    assert_ok(&run(&cfg, &["add", "tasks"]), "add tasks fill enabled");
    let text = fs::read_to_string(&cfg).unwrap();
    // Real table once (commented header still present as comment)
    assert_eq!(
        text.lines().filter(|l| l.trim() == "[tasks]").count(),
        1,
        "must not add second [tasks]:\n{text}"
    );
    // Locate the real `[tasks]` table (not the `# [tasks]` comment).
    let mut tasks_body = String::new();
    let mut in_tasks = false;
    for line in text.lines() {
        if line.trim() == "[tasks]" {
            in_tasks = true;
            continue;
        }
        if in_tasks {
            if line.trim().starts_with('[') && line.trim().ends_with(']') {
                break;
            }
            tasks_body.push_str(line);
            tasks_body.push('\n');
        }
    }
    assert!(
        tasks_body.lines().any(|l| l.trim() == "enabled = true"),
        "expected enabled=true in:\n{tasks_body}\nfull:\n{text}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// Disabled upstream is flagged in `list mcp`; get shows sidecar_spec after probe.
#[test]
fn flow_list_disabled_and_get_shows_sidecar() {
    let dir = tmp_dir("disabled");
    let cfg = scaffold(&dir);
    let mock = mock_bin();

    assert_ok(
        &run(
            &cfg,
            &[
                "add",
                "mcp",
                "--transport",
                "stdio",
                "mock",
                "--",
                mock.to_str().unwrap(),
            ],
        ),
        "add mcp probe",
    );

    // Flip enabled=false in registry
    let mut reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    reg.upstreams[0].enabled = false;
    vmcp_registry::save_registry_atomic(&dir.join("registry.json"), &reg).unwrap();

    let list = run(&cfg, &["list", "mcp"]);
    assert_ok(&list, "list disabled");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("mock"), "{stdout}");
    assert!(stdout.contains("[disabled]"), "{stdout}");

    let get = run(&cfg, &["get", "mcp", "mock"]);
    assert_ok(&get, "get mcp");
    let g = String::from_utf8_lossy(&get.stdout);
    assert!(g.contains("sidecar_spec"), "{g}");
    assert!(g.contains("mock.json"), "{g}");
    assert!(
        g.contains("\"enabled\": false") || g.contains("\"enabled\":false"),
        "{g}"
    );

    let _ = fs::remove_dir_all(&dir);
}
