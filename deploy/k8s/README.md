# vmcp on Kubernetes (Gateway API)

Minimal manifests for a **single-replica** gateway. Not HA (RWO PVC + in-memory
upstream pool). Not a CRD operator.

## Important truths

| Topic | Reality |
| ----- | ------- |
| Registry writes | **Operator / ConfigMap** owns `registry.json`. vmcp watches + reloads; it does not write the CM. Prefer `POST /api/v1/upstreams/reload` after apply (inotify on CM `..data` is best-effort). |
| Image | Set `REPLACE_WITH_BUILT_IMAGE` to a build from **this branch** (main/`v1.0.x` may lack `/api/v1`). |
| Tokens | Must be on the **PVC** (writable). Run `job-bootstrap-tokens.yaml` once. RO Secret mounts break token CRUD. |
| Stdio upstreams | Runtime image has no Node/`uv` — use **HTTP** upstreams in cluster. |
| `/api/v1` | Only when `auth.enabled` + `tokens_file`; requires Bearer `mcp:admin` (also full MCP). Keep on **internal** HTTPRoute. |
| `/ready` | Soft: 503 if registry has enabled upstreams and none are connected. |

## Apply order

```bash
kubectl create ns vmcp
# create Secret vmcp-auth (see secret.example.yaml)
kubectl apply -f pvc.yaml -f configmap.yaml -f service.yaml
kubectl apply -f job-bootstrap-tokens.yaml
kubectl apply -f deployment.yaml
# optional:
kubectl apply -f httproute.example.yaml -f networkpolicy.example.yaml
```

## Operator reconcile loop

1. Update Upstream CM / rewrite mounted `registry.json`.
2. `POST /api/v1/upstreams/reload` with admin Bearer; check `registry_sha256` / `mtime_unix_ms`.
3. Mint agent tokens via `POST /api/v1/tokens` (or `PUT …/tokens/:id` to rotate).
