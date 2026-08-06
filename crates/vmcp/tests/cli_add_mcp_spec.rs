//! `vmcp add mcp` probes tools/list and writes specs/<name>.json.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use vmcp_registry::{load_registry_raw, load_sidecar};

fn tmp_dir(stem: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("vmcp-cli-probe-{stem}-{nanos}"));
    fs::create_dir_all(&p).unwrap();
    p
}

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

#[test]
fn add_mcp_probes_tools_list_and_writes_sidecar() {
    let dir = tmp_dir("ok");
    let cfg = abs_config(&dir);
    fs::create_dir_all(dir.join("specs")).unwrap();
    fs::create_dir_all(dir.join("skills")).unwrap();
    fs::write(dir.join("registry.json"), "{\n  \"upstreams\": []\n}\n").unwrap();

    let status = Command::new(vmcp_bin())
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "add",
            "mcp",
            "--transport",
            "stdio",
            "mock",
            "--",
            mock_bin().to_str().unwrap(),
        ])
        .status()
        .expect("spawn vmcp");
    assert!(status.success(), "vmcp add mcp failed: {status}");

    let reg = load_registry_raw(&dir.join("registry.json")).unwrap();
    assert_eq!(reg.upstreams.len(), 1);
    assert_eq!(
        reg.upstreams[0].sidecar_spec.as_deref(),
        Some(Path::new("mock.json"))
    );

    let sc = load_sidecar(Some(&dir.join("specs/mock.json")))
        .unwrap()
        .expect("sidecar written");
    let names: Vec<_> = sc.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"delay_read"), "got {names:?}");
    assert!(names.contains(&"delay_write"), "got {names:?}");

    let _ = fs::remove_dir_all(&dir);
}
