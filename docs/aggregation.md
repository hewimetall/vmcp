# GraphQL-агрегация upstream-инструментов

vmcp строит динамическую GraphQL-схему из `tools/list` всех upstream MCP
серверов. Режим агрегации задаётся не отдельным флагом, а типом GraphQL
операции, который выводится из `readOnlyHint` инструмента.

| Аннотация инструмента | Куда попадает в схеме | Как исполняется |
| --------------------- | --------------------- | --------------- |
| `readOnlyHint = true` | `Query.<server>`      | параллельно     |
| `readOnlyHint = false` или отсутствует | `Mutation.<server>` | последовательно |

Разбиение выполняется при сборке схемы: читающие инструменты попадают в
`Query`, пишущие - в `Mutation`.

```rust
let (reads, writes): (Vec<_>, Vec<_>) = tools.into_iter().partition(|t| t.read_only);
// reads  -> Query.<server>
// writes -> Mutation.<server>
```

## Query: параллельное разветвление вызовов

Поля одной GraphQL-операции `query` могут резолвиться конкурентно. В vmcp каждый
инструмент является async-резолвером, который вызывает upstream через
`pool.call(server, tool, args)`. Если в одном документе несколько алиасов к
разным upstream-серверам, шлюз запускает вызовы параллельно и возвращает один
GraphQL-ответ.

```graphql
{
  moscow: time { getCurrentTime(timezone: "Europe/Moscow") { json } }
  tokyo: time { getCurrentTime(timezone: "Asia/Tokyo") { json } }
  customers: postgres { query(sql: "SELECT name, country FROM customers") { json } }
}
```

Граница параллелизма - upstream-сессия. Внутри одного upstream вызовы защищены
`call_lock`, потому что за сервером обычно стоит один stdio-пайп. Два алиаса к
разным серверам выполняются параллельно; два алиаса к одному серверу на границе
сессии выстраиваются в очередь. Для одного SQL-upstream лучше упаковывать
связанные чтения в один SQL-запрос (`UNION ALL`, `JOIN`, `GROUP BY`, `CASE`).

## Mutation: последовательные побочные эффекты

Поля верхнего уровня операции `mutation` по спецификации GraphQL исполняются
строго последовательно. Поэтому пишущие инструменты vmcp агрегируются серийно
даже при обращении к разным upstream-серверам.

```graphql
mutation {
  a: jira { createIssue(project: "OPS", summary: "...") { json } }
  b: postgres { insertAudit(event: "issue_created") { json } }
}
```

В этом примере `b` стартует только после полного завершения `a`. Такой режим
используется для операций с побочными эффектами, где важен порядок.

## Проверка агрегации

Поведение покрыто end-to-end тестом `crates/vmcp/tests/aggregation.rs`. Тест
поднимает два настоящих stdio-upstream (`alpha` и `beta`) на базе
`crates/vmcp/src/bin/mock_delay_upstream.rs`. Заглушка отдаёт `delay_read` и
`delay_write`, спит заданное число миллисекунд и возвращает окно обслуживания
вызова (`start_us` / `end_us`). По пересечению окон видно, были ли вызовы
параллельными.

Запуск:

```bash
cargo test -p vmcp --test aggregation
```

Для просмотра диагностических строк:

```bash
cargo test -p vmcp --test aggregation -- --nocapture --test-threads=1
```

Ожидаемая форма вывода:

```text
PARALLEL reads:     alpha=[..514542..816506] beta=[..514601..815537] wall=303ms (2x300ms sleeps)
SEQUENTIAL writes:  alpha=[..828341..130088] beta=[..132632..434529] wall=608ms (2x300ms sleeps)
```

Что проверяется:

- `reads_aggregate_in_parallel`: окна `alpha` и `beta` пересекаются, общее время
  близко к одной задержке.
- `writes_aggregate_sequentially`: окна не пересекаются, порядок сохранён,
  общее время близко к сумме двух задержек.

## Долгие задачи и HTTP upstreams

Агрегация выше относится к одному синхронному GraphQL-документу. Для долгих
прогонов используйте нативные MCP Tasks через `run_task` и SEP-1686; настройка
`[tasks]`, списка разрешённых `task_support` инструментов и SQLite TaskStore описаны в
[tasks.md](tasks.md).

HTTP upstream подключается через `transport = "http"` и `url` на Streamable HTTP
MCP-эндпоинт; секреты передавайте через переменные окружения и `bearer`.

```json
{
  "name": "remote",
  "transport": "http",
  "url": "http://127.0.0.1:8080/mcp",
  "bearer": "${REMOTE_MCP_TOKEN}"
}
```
