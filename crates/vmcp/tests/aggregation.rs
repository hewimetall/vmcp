//! End-to-end proof of vmcp's two aggregation modes.
//!
//! vmcp fans every upstream MCP server out behind one GraphQL document. How
//! that document is *aggregated* is decided by the GraphQL operation type,
//! which in turn is decided by each tool's `readOnlyHint`:
//!
//! * **Read** tools land under `Query`  → resolved **in parallel**.
//! * **Write** tools land under `Mutation` → resolved **sequentially**.
//!
//! These tests spawn two real, independent stdio upstreams (`alpha`, `beta`)
//! via [`mock_delay_upstream`], each exposing a `delay_read`/`delay_write`
//! tool that sleeps and then reports the wall-clock window it was served in.
//! Because both processes share the host clock, comparing the windows is a
//! black-box proof of concurrency:
//!
//! * Two aliased **reads** on different upstreams produce **overlapping**
//!   windows — they ran at the same time (parallel fan-out).
//! * Two aliased **writes** produce **disjoint, ordered** windows — one fully
//!   finished before the next began (serial fan-out, per the GraphQL spec).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_graphql::Request;
use serde_json::Value;
use vmcp_graphql::{build_schema, SchemaLimits};
use vmcp_notify::Bus;
use vmcp_registry::{Registry, SidecarSpec, SidecarTool, UpstreamSpec};
use vmcp_upstream::UpstreamPool;

/// One served-call window in microseconds since the Unix epoch, as reported
/// by the upstream itself.
#[derive(Debug, Clone, Copy)]
struct Window {
    start_us: u64,
    end_us: u64,
}

impl Window {
    fn overlaps(&self, other: &Window) -> bool {
        self.start_us < other.end_us && other.start_us < self.end_us
    }
}

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new(prefix: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("{prefix}-{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write a sidecar that buckets `delay_read`→Query and `delay_write`→Mutation
/// and return its absolute path.
fn write_sidecar(dir: &std::path::Path, server: &str) -> std::path::PathBuf {
    let spec = SidecarSpec {
        server: server.to_string(),
        tools: vec![
            SidecarTool {
                name: "delay_read".into(),
                read_only: true,
                description: None,
                task_support: Some(vmcp_registry::TaskSupportHint::Optional),
            },
            SidecarTool {
                name: "delay_write".into(),
                read_only: false,
                description: None,
                task_support: None,
            },
        ],
    };
    let path = dir.join(format!("{server}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();
    path
}

fn upstream(name: &str, exe: &str, sidecar: std::path::PathBuf) -> UpstreamSpec {
    let mut env = std::collections::BTreeMap::new();
    env.insert("MOCK_LABEL".to_string(), name.to_string());
    UpstreamSpec {
        name: name.to_string(),
        description: Some(format!("mock delay upstream {name}")),
        transport: vmcp_registry::UpstreamTransport::Stdio,
        url: None,
        bearer: None,
        command: exe.to_string(),
        args: vec![],
        env,
        cwd: None,
        sidecar_spec: Some(sidecar),
        enabled: true,
    }
}

async fn boot_pool(dir: &std::path::Path) -> Arc<UpstreamPool> {
    let exe = env!("CARGO_BIN_EXE_mock_delay_upstream");
    let registry = Registry {
        upstreams: vec![
            upstream("alpha", exe, write_sidecar(dir, "alpha")),
            upstream("beta", exe, write_sidecar(dir, "beta")),
        ],
    };
    let bus = Bus::new(1024);
    let (pool, failures) = UpstreamPool::spawn_all(
        &registry,
        bus,
        Some(dir),
        Duration::from_secs(30),
        Duration::from_secs(30),
    )
    .await;
    assert!(failures.is_empty(), "upstream spawn failures: {failures:?}");
    let mut names = pool.names();
    names.sort();
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    Arc::new(pool)
}

fn window_of(data: &Value, alias: &str, tool_field: &str) -> Window {
    let json = &data[alias][tool_field]["json"];
    let start_us = json["start_us"]
        .as_u64()
        .unwrap_or_else(|| panic!("missing start_us for {alias}.{tool_field}; got: {json}"));
    let end_us = json["end_us"]
        .as_u64()
        .unwrap_or_else(|| panic!("missing end_us for {alias}.{tool_field}; got: {json}"));
    Window { start_us, end_us }
}

async fn run(pool: &Arc<UpstreamPool>, doc: &str) -> (Value, Duration) {
    let schema = build_schema(pool.all_resolved(), pool.clone(), SchemaLimits::default())
        .expect("build schema");
    let started = Instant::now();
    let resp = schema.execute(Request::new(doc.to_string())).await;
    let elapsed = started.elapsed();
    let body = serde_json::to_value(&resp).unwrap();
    assert!(
        body.get("errors").map(|e| e.is_null()).unwrap_or(true),
        "graphql errors: {body}"
    );
    (body, elapsed)
}

const SLEEP_MS: u64 = 300;

/// Two aliased **reads** on different upstreams run concurrently: their
/// served windows overlap, and the whole document finishes in roughly one
/// sleep, not two.
#[tokio::test]
async fn reads_aggregate_in_parallel() {
    let dir = TempDir::new("vmcp-agg-parallel");
    let pool = boot_pool(dir.path()).await;

    let doc = format!(
        r#"{{
            a: alpha {{ delayRead(ms: {SLEEP_MS}) {{ json }} }}
            b: beta  {{ delayRead(ms: {SLEEP_MS}) {{ json }} }}
        }}"#
    );
    let (body, elapsed) = run(&pool, &doc).await;
    let data = &body["data"];
    let a = window_of(data, "a", "delayRead");
    let b = window_of(data, "b", "delayRead");

    eprintln!(
        "PARALLEL reads: alpha=[{}..{}] beta=[{}..{}] wall={}ms (2×{}ms sleeps)",
        a.start_us,
        a.end_us,
        b.start_us,
        b.end_us,
        elapsed.as_millis(),
        SLEEP_MS
    );

    assert!(
        a.overlaps(&b),
        "read windows must overlap (parallel fan-out): alpha={a:?} beta={b:?}"
    );
    // Wall clock proves it too: parallel ≈ one sleep, serial would be ≈ two.
    assert!(
        elapsed < Duration::from_millis(SLEEP_MS * 2),
        "parallel reads took {}ms, expected well under {}ms",
        elapsed.as_millis(),
        SLEEP_MS * 2
    );

    pool.shutdown().await;
}

/// Two aliased **writes** run serially: per the GraphQL spec, top-level
/// mutation fields are executed one after another, so the served windows are
/// disjoint and ordered, and the document takes roughly two sleeps.
#[tokio::test]
async fn writes_aggregate_sequentially() {
    let dir = TempDir::new("vmcp-agg-serial");
    let pool = boot_pool(dir.path()).await;

    let doc = format!(
        r#"mutation {{
            a: alpha {{ delayWrite(ms: {SLEEP_MS}) {{ json }} }}
            b: beta  {{ delayWrite(ms: {SLEEP_MS}) {{ json }} }}
        }}"#
    );
    let (body, elapsed) = run(&pool, &doc).await;
    let data = &body["data"];
    let a = window_of(data, "a", "delayWrite");
    let b = window_of(data, "b", "delayWrite");

    eprintln!(
        "SEQUENTIAL writes: alpha=[{}..{}] beta=[{}..{}] wall={}ms (2×{}ms sleeps)",
        a.start_us,
        a.end_us,
        b.start_us,
        b.end_us,
        elapsed.as_millis(),
        SLEEP_MS
    );

    assert!(
        !a.overlaps(&b),
        "write windows must NOT overlap (serial fan-out): alpha={a:?} beta={b:?}"
    );
    // The second field starts only after the first finishes.
    let first_end = a.end_us.min(b.end_us);
    let second_start = a.start_us.max(b.start_us);
    assert!(
        second_start >= first_end,
        "second mutation started before first finished: alpha={a:?} beta={b:?}"
    );
    // Wall clock proves it: serial ≈ two sleeps.
    assert!(
        elapsed >= Duration::from_millis(SLEEP_MS * 2 - 60),
        "serial writes took only {}ms, expected ≳ {}ms",
        elapsed.as_millis(),
        SLEEP_MS * 2
    );

    pool.shutdown().await;
}
