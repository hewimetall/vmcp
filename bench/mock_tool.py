"""Mock `query_graphql` tool: takes a GraphQL document (as the LLM wrote it),
returns plausible JSON shaped like the real tool's `{"data": ..., "errors":
null}`.

Goal: results are SUFFICIENT to answer the canonical demo task in ONE batched
call. If the LLM splits into N calls, it spends N turns — that's the signal
we measure. Don't return "missing field" errors or hints: that would bias the
model toward (or away from) aliasing.
"""

from __future__ import annotations

import json
import re
from datetime import datetime, timedelta, timezone
from typing import Any

# ---------- static fixtures ----------

# Fake row counts per table — fixed so multiple runs produce identical numbers
# (variance in n_tool_calls is what we measure, not data noise).
TABLE_COUNTS: dict[str, int] = {
    "customers": 12,
    "departments": 5,
    "employees": 87,
    "orders": 230,
}

TOP_SALARIES = [
    {"name": "Ivan Petrov", "salary": 480000, "department": "Engineering"},
    {"name": "Anna Sokolova", "salary": 415000, "department": "Sales"},
    {"name": "Dmitri Volkov", "salary": 392000, "department": "Engineering"},
    {"name": "Maria Kuznetsova", "salary": 370000, "department": "Finance"},
    {"name": "Pavel Orlov", "salary": 355000, "department": "Engineering"},
]

CUSTOMERS_WITH_COUNTRY = [
    {"id": 1, "name": "Acme GmbH", "country": "DE"},
    {"id": 2, "name": "Sakura KK", "country": "JP"},
    {"id": 3, "name": "Volga Trade", "country": "RU"},
    {"id": 4, "name": "Mosaic LLC", "country": "RU"},
    {"id": 5, "name": "Berlin Bytes", "country": "DE"},
    {"id": 6, "name": "Tokyo Logistics", "country": "JP"},
]

DEPARTMENTS_ALPHA = [
    {"id": 4, "name": "Engineering"},
    {"id": 1, "name": "Finance"},
    {"id": 3, "name": "HR"},
    {"id": 2, "name": "Operations"},
    {"id": 5, "name": "Sales"},
]

ORDERS_BY_EMPLOYEE = [
    {"employee": "Ivan Petrov", "total_orders": 41, "total_value": 1_280_000},
    {"employee": "Anna Sokolova", "total_orders": 38, "total_value": 1_104_500},
    {"employee": "Pavel Orlov", "total_orders": 33, "total_value": 925_000},
    {"employee": "Dmitri Volkov", "total_orders": 29, "total_value": 870_300},
    {"employee": "Maria Kuznetsova", "total_orders": 27, "total_value": 760_200},
    {"employee": "Sergei Ivanov", "total_orders": 22, "total_value": 510_000},
    {"employee": "Olga Vasilieva", "total_orders": 18, "total_value": 444_000},
    {"employee": "Yuri Belov", "total_orders": 17, "total_value": 410_000},
    {"employee": "Elena Frolova", "total_orders": 15, "total_value": 380_000},
    {"employee": "Nikita Zaitsev", "total_orders": 12, "total_value": 290_000},
]

# IANA tz → fixed offset for the mock. Real vmcp would compute live time;
# fixed values keep runs reproducible.
TZ_OFFSETS: dict[str, int] = {
    "Europe/Moscow": 3,
    "Europe/Berlin": 2,
    "Asia/Tokyo": 9,
    "UTC": 0,
    "Etc/UTC": 0,
}
BASE_TIME = datetime(2026, 5, 28, 12, 0, 0, tzinfo=timezone.utc)


# ---------- alias / field extraction ----------

_ALIAS_RE = re.compile(r"(\w+)\s*:\s*(\w+)\s*\{")
_BARE_TOP_RE = re.compile(r"\{\s*(\w+)\s*\{")


def extract_top_level_aliases(query: str) -> list[tuple[str, str]]:
    """Return list of (alias_name, server_name) for top-level aliased fields,
    plus (server_name, server_name) entries for bare (unaliased) top-level
    fields. Best-effort regex parse — sufficient for the metric, no GraphQL
    grammar dependency.
    """
    body = query.strip().lstrip("query").lstrip("mutation").strip()
    if not body.startswith("{"):
        return []
    # Find aliases inside the outermost block. We do a shallow scan: every
    # `<alias>: <server> {` occurrence at any depth — overcount risk is low
    # because aliases are mostly used at top level in this domain.
    aliases = _ALIAS_RE.findall(body)
    # Bare top-level fields (no alias) — only match the immediate inside of
    # the very first `{`. Doesn't try to handle nested namespaces.
    inner = body[body.index("{") + 1 :]
    bare_top = _BARE_TOP_RE.findall("{" + inner.split("}")[0])
    # Combine; alias entries already cover the bare ones if any.
    if aliases:
        return aliases
    return [(s, s) for s in bare_top]


# ---------- response synthesis ----------


def _time_response(timezone_arg: str) -> dict[str, Any]:
    offset_hours = TZ_OFFSETS.get(timezone_arg, 0)
    t = BASE_TIME + timedelta(hours=offset_hours)
    iso = t.strftime(f"%Y-%m-%dT%H:%M:%S{offset_hours:+03d}:00")
    return {"timezone": timezone_arg, "datetime": iso}


def _postgres_response(sql: str) -> Any:
    """Best-effort SQL → fixture mapping. Handles the common patterns the LLM
    emits when packing multiple sub-questions into one query:
      • multi-subquery counts:  SELECT (SELECT COUNT(*) FROM X) AS x, ...
      • UNION ALL of labelled counts
      • single-table COUNT(*) / COUNT(1)
      • ORDER BY salary DESC LIMIT N
      • customers × country joins
      • departments listing
      • orders join with totals
    Anything unrecognised → `[{"note":"mock", "sql":...}]` so the LLM at least
    sees a non-error.
    """
    s = sql.lower()

    # 1. Multi-subquery counts:
    #    SELECT (SELECT COUNT(*) FROM customers) AS customers, ...
    sub_count_re = re.compile(
        r"\(\s*select\s+count\s*\(\s*\*\s*\)\s+from\s+(\w+)\s*\)\s+as\s+(\w+)"
    )
    subs = sub_count_re.findall(s)
    if subs:
        row: dict[str, Any] = {}
        for table, alias in subs:
            row[alias] = TABLE_COUNTS.get(table.strip(), 0)
        return [row]

    # 2. UNION ALL of labelled counts:
    #    SELECT 'customers' AS label, COUNT(*) FROM customers UNION ALL ...
    if "union all" in s and "count" in s:
        rows: list[dict[str, Any]] = []
        for m in re.finditer(
            r"select\s+['\"](?P<label>\w+)['\"][^a-z]*(?:as\s+\w+)?\s*,\s*"
            r"count\s*\(\s*\*\s*\)[^f]*from\s+(?P<table>\w+)",
            s,
        ):
            rows.append(
                {"label": m.group("label"), "count": TABLE_COUNTS.get(m.group("table"), 0)}
            )
        if rows:
            return rows

    # 3. WITH cte AS (...) — pull out first table count as fallback
    if s.lstrip().startswith("with") and "count" in s:
        for table, n in TABLE_COUNTS.items():
            if table in s:
                return [{"count": n, "table": table}]

    # 4. Single-table COUNT(*) / COUNT(1) — first table mentioned wins
    if "count(*)" in s or "count(1)" in s:
        for table, n in TABLE_COUNTS.items():
            if table in s:
                return [{"count": n}]
        return [{"count": 0}]

    # 5. Top-paid (ORDER BY salary DESC, with or without LIMIT)
    if ("salary" in s and ("desc" in s or "highest" in s or "top" in s)) or (
        "salary" in s and "limit" in s
    ):
        m = re.search(r"limit\s+(\d+)", s)
        n = int(m.group(1)) if m else 5
        return TOP_SALARIES[:n]

    # 6. Orders × employees join
    if "orders" in s and ("join" in s or "group by" in s or "total" in s):
        m = re.search(r"limit\s+(\d+)", s)
        n = int(m.group(1)) if m else 10
        return ORDERS_BY_EMPLOYEE[:n]

    # 7. Customer breakdown by country (GROUP BY country)
    if "country" in s and "group by" in s:
        breakdown: dict[str, int] = {}
        for c in CUSTOMERS_WITH_COUNTRY:
            breakdown[c["country"]] = breakdown.get(c["country"], 0) + 1
        return [{"country": k, "count": v} for k, v in sorted(breakdown.items())]

    # 8. Customers listed with country (any SELECT touching both)
    if "customers" in s and "country" in s:
        return CUSTOMERS_WITH_COUNTRY

    # 9. Departments listing (alphabetical or not)
    if "departments" in s:
        return DEPARTMENTS_ALPHA

    # 10. Bare employees query → return top salaries as a plausible default
    if "employees" in s:
        return TOP_SALARIES

    # 11. Bare customers query → return list
    if "customers" in s:
        return CUSTOMERS_WITH_COUNTRY

    return [{"note": "mock", "sql": sql[:120]}]


def _servers_payload() -> dict[str, Any]:
    return {
        "servers": [
            {"name": "postgres", "description": "Demo PostgreSQL (customers, departments, employees, orders)", "toolCount": 4, "readOnlyCount": 3},
            {"name": "time", "description": "IANA timezone clock", "toolCount": 1, "readOnlyCount": 1},
            {"name": "agentmemory", "description": "Semantic memory store", "toolCount": 7, "readOnlyCount": 4},
        ]
    }


def respond(args: dict[str, Any]) -> str:
    """Take the parsed tool-call args (`{query, variables?, operation_name?}`)
    and return a JSON string to feed back as the tool result.
    """
    query: str = args.get("query", "") if isinstance(args, dict) else ""
    if not query:
        return json.dumps({"data": None, "errors": [{"message": "missing query"}]})

    # Discovery probes
    if re.search(r"\bservers\s*\{", query) or "__type" in query or "__schema" in query:
        return json.dumps({"data": _servers_payload(), "errors": None})

    # Build a response object that mirrors the requested shape: for every
    # top-level alias, place a corresponding field in `data` with json-encoded
    # contents.
    data: dict[str, Any] = {}
    added_time = False
    added_postgres = False

    # Aliased `<alias>: time { getCurrentTime(timezone: "X") }` blocks
    for m in re.finditer(
        r"(?P<alias>\w+)\s*:\s*time\s*\{[^}]*getCurrentTime\s*\(\s*timezone:\s*\"(?P<tz>[^\"]+)\"",
        query,
    ):
        data[m.group("alias")] = {
            "time": {"getCurrentTime": {"json": json.dumps(_time_response(m.group("tz")))}}
        }
        added_time = True
    # Aliased `<alias>: postgres { query(sql: ...) }` blocks
    for m in re.finditer(
        r"(?P<alias>\w+)\s*:\s*postgres\s*\{[^}]*query\s*\(\s*sql:\s*(?:\"\"\"(?P<sql_block>.*?)\"\"\"|\"(?P<sql>(?:[^\"\\]|\\.)*)\")",
        query,
        re.DOTALL,
    ):
        sql = (m.group("sql_block") or m.group("sql") or "").replace('\\"', '"')
        rows = _postgres_response(sql)
        data[m.group("alias")] = {
            "postgres": {"query": {"json": json.dumps(rows), "text": None, "isError": False}}
        }
        added_postgres = True

    # Bare (unaliased) top-level time call — only if no alias matched
    if not added_time and "getCurrentTime" in query:
        m = re.search(r"getCurrentTime\s*\(\s*timezone:\s*\"([^\"]+)\"", query)
        if m:
            data["time"] = {
                "getCurrentTime": {"json": json.dumps(_time_response(m.group(1)))}
            }
    # Bare (unaliased) top-level postgres call
    if not added_postgres and re.search(r"\bpostgres\s*\{", query):
        m = re.search(
            r"postgres\s*\{[^}]*query\s*\(\s*sql:\s*"
            r"(?:\"\"\"(?P<sql_block>.*?)\"\"\"|\"(?P<sql>(?:[^\"\\]|\\.)*)\")",
            query,
            re.DOTALL,
        )
        if m:
            sql = (m.group("sql_block") or m.group("sql") or "").replace('\\"', '"')
            data["postgres"] = {
                "query": {"json": json.dumps(_postgres_response(sql)), "text": None, "isError": False}
            }

    if not data:
        return json.dumps({"data": {}, "errors": None})
    return json.dumps({"data": data, "errors": None})


# ---------- self-tests ----------

if __name__ == "__main__":
    # Light smoke: should not raise; should produce non-empty data.
    samples = [
        '{ moscow: time { getCurrentTime(timezone: "Europe/Moscow") { json } } tokyo: time { getCurrentTime(timezone: "Asia/Tokyo") { json } } customers: postgres { query(sql: "SELECT name, country FROM customers") { json } } }',
        '{ servers { name description toolCount } }',
        '{ countCustomers: postgres { query(sql: "SELECT COUNT(*) FROM customers") { json } } topPaid: postgres { query(sql: "SELECT name, salary FROM employees ORDER BY salary DESC LIMIT 3") { json } } }',
    ]
    for s in samples:
        r = respond({"query": s})
        parsed = json.loads(r)
        print("query:", s[:80], "...")
        print("aliases:", extract_top_level_aliases(s))
        print("response keys:", list((parsed.get("data") or {}).keys()))
        print()
