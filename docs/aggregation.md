# GraphQL aggregation of upstream tools

**Language:** English | [Русский](ru/aggregation.md)

vmcp builds a dynamic GraphQL schema from the `tools/list` response of every
upstream MCP server. The aggregation mode is determined not by a separate flag
but by the GraphQL operation type, which is inferred from the tool's
`readOnlyHint`.

| Tool annotation | Schema location | Execution |
| --------------- | --------------- | --------- |
| `readOnlyHint = true` | `Query.<server>` | Parallel |
| `readOnlyHint = false` or absent | `Mutation.<server>` | Sequential |

The tools are partitioned when the schema is built: read-only tools go into
`Query`, while write tools go into `Mutation`.

```rust
let (reads, writes): (Vec<_>, Vec<_>) = tools.into_iter().partition(|t| t.read_only);
// reads  -> Query.<server>
// writes -> Mutation.<server>
```

## Query: parallel call fan-out

Fields in a single GraphQL `query` operation can resolve concurrently. In vmcp,
each tool is an async resolver that calls the upstream through
`pool.call(server, tool, args)`. If one document contains multiple aliases that
target different upstream servers, the gateway runs the calls in parallel and
returns a single GraphQL response.

```graphql
{
  moscow: time { getCurrentTime(timezone: "Europe/Moscow") { json } }
  tokyo: time { getCurrentTime(timezone: "Asia/Tokyo") { json } }
  customers: postgres { query(sql: "SELECT name, country FROM customers") { json } }
}
```

The upstream session is the concurrency boundary. Calls within one upstream are
protected by `call_lock`, because a server usually has a single stdio pipe
behind it. Two aliases targeting different servers run in parallel; two aliases
targeting the same server are queued at the session boundary. For a single SQL
upstream, prefer combining related reads into one SQL query with `UNION ALL`,
`JOIN`, `GROUP BY`, or `CASE`.

## Mutation: sequential side effects

The GraphQL specification requires top-level fields in a `mutation` operation
to run strictly in sequence. vmcp therefore aggregates write tools
sequentially, even when they target different upstream servers.

```graphql
mutation {
  a: jira { createIssue(project: "OPS", summary: "...") { json } }
  b: postgres { insertAudit(event: "issue_created") { json } }
}
```

In this example, `b` starts only after `a` has finished completely. This mode is
used for side-effecting operations where order matters.

## Verifying aggregation

This behavior is covered by the end-to-end test in
`crates/vmcp/tests/aggregation.rs`. The test starts two real stdio upstreams,
`alpha` and `beta`, based on `crates/vmcp/src/bin/mock_delay_upstream.rs`. The
stub exposes `delay_read` and `delay_write`, sleeps for the specified number of
milliseconds, and returns the call's execution window (`start_us` / `end_us`).
Whether the windows overlap shows whether the calls ran in parallel.

Run it with:

```bash
cargo test -p vmcp --test aggregation
```

To display the diagnostic output:

```bash
cargo test -p vmcp --test aggregation -- --nocapture --test-threads=1
```

Expected output:

```text
PARALLEL reads:     alpha=[..514542..816506] beta=[..514601..815537] wall=303ms (2x300ms sleeps)
SEQUENTIAL writes:  alpha=[..828341..130088] beta=[..132632..434529] wall=608ms (2x300ms sleeps)
```

The test verifies that:

- `reads_aggregate_in_parallel`: the `alpha` and `beta` windows overlap, and the
  total duration is close to a single delay.
- `writes_aggregate_sequentially`: the windows do not overlap, their order is
  preserved, and the total duration is close to the sum of both delays.

## Long-running tasks and HTTP upstreams

The aggregation described above applies to a single synchronous GraphQL
document. For long-running operations, use native MCP Tasks through `run_task`
and SEP-1686. The `[tasks]` configuration, allowlist of `task_support` tools,
and SQLite TaskStore are documented in [tasks.md](tasks.md).

Configure an HTTP upstream with `transport = "http"` and a `url` pointing to a
Streamable HTTP MCP endpoint. Pass secrets through environment variables and
`bearer`.

```json
{
  "name": "remote",
  "transport": "http",
  "url": "http://127.0.0.1:8080/mcp",
  "bearer": "${REMOTE_MCP_TOKEN}"
}
```

## `query_graphql` response format

MCP tool text defaults to a compact JSON envelope. The optional
`[gql].gcf = true` flag (env `VMCP_GQL__GCF`) encodes the same envelope using
the [GCF](https://gcformat.com/) generic profile. The separate `[proxy].gcf`
flag (env `VMCP_PROXY__GCF`) does the same for `/mcp-proxy`.
See [builds-and-modes.md](builds-and-modes.md#gcf-output-optional-two-flags).
