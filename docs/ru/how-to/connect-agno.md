# Как подключить агента Agno к vmcp

**Язык:** [English](../../how-to/connect-agno.md) | Русский

Подключите агента Agno к vmcp по Streamable HTTP и авторизуйте его с помощью
статического bearer-токена с ограниченным scope.

## Предварительные требования

- Шлюз vmcp доступен по адресу вида
  `https://gateway.example.com/mcp`
- У вас есть доступ к настроенному на шлюзе `auth.tokens_file` или статический
  токен от оператора шлюза
- Python 3.12 и [`uv`](https://docs.astral.sh/uv/)
- API-ключ OpenAI для агента из примера

## Шаги

### 1. Выпустите токен для Agno

На хосте vmcp создайте токен со scope `mcp:use`:

```bash
vmcp pre-reg --name agno --scope mcp:use --out ./tokens.json
```

Если это первый статический токен, укажите тот же файл в `auth.tokens_file` и
один раз перезапустите vmcp:

```toml
[auth]
tokens_file = "./tokens.json"
```

Безопасно передайте напечатанный токен `vmcp_...` на машину с Agno. Считайте
его секретом.

### 2. Установите Agno с поддержкой MCP

Создайте виртуальное окружение и установите необходимые пакеты:

```bash
uv venv --python 3.12
source .venv/bin/activate
uv pip install -U "agno[mcp]" openai
```

### 3. Экспортируйте параметры подключения

Задайте URL шлюза, токен vmcp и ключ провайдера модели:

```bash
export VMCP_URL='https://gateway.example.com/mcp'
export VMCP_TOKEN='vmcp_...'
export OPENAI_API_KEY='sk-...'
```

Не помещайте эти значения непосредственно в Python-файл.

### 4. Создайте агента Agno

Сохраните пример как `agno_vmcp.py`:

```python
import asyncio
import os

from agno.agent import Agent
from agno.models.openai import OpenAIResponses
from agno.tools.mcp import MCPTools, StreamableHTTPClientParams


async def main() -> None:
    server_params = StreamableHTTPClientParams(
        url=os.environ["VMCP_URL"],
        headers={
            "Authorization": f"Bearer {os.environ['VMCP_TOKEN']}",
        },
        timeout=30,
        sse_read_timeout=300,
        terminate_on_close=True,
    )

    async with MCPTools(
        server_params=server_params,
        transport="streamable-http",
    ) as vmcp_tools:
        tool_names = {tool.name for tool in vmcp_tools.functions.values()}
        if "query_graphql" not in tool_names:
            raise RuntimeError(f"query_graphql not found; got {sorted(tool_names)}")

        agent = Agent(
            model=OpenAIResponses(id="gpt-5.2"),
            tools=[vmcp_tools],
            markdown=True,
        )
        await agent.aprint_response(
            input=(
                "Call query_graphql exactly once with "
                "`{ servers { name description toolCount readOnlyCount } }`. "
                "Return the result as a table."
            ),
            stream=True,
            markdown=True,
        )


if __name__ == "__main__":
    asyncio.run(main())
```

### 5. Запустите агента

Выполните:

```bash
python agno_vmcp.py
```

Скрипт сначала проверит, что Agno обнаружил `query_graphql`, затем попросит
модель вывести подключенные к vmcp upstream-серверы.

## Как проверить результат

- Скрипт не выбрасывает ошибку `query_graphql not found`.
- Агент возвращает таблицу с upstream-именами из `registry.json`.
- После первого MCP-запроса клиент Agno появляется на странице vmcp
  **Sessions**.

## Устранение неполадок

| Проблема | Решение |
| -------- | ------- |
| Клиент возвращает 401 | Проверьте, что `VMCP_TOKEN` совпадает с токеном в активном `auth.tokens_file`. |
| Клиент не подключается | Проверьте `VMCP_URL`: нужен Streamable HTTP endpoint `/mcp`, а не `/mcp-proxy` или `/admin`. |
| `query_graphql` отсутствует | Убедитесь, что подключились к самому vmcp и успешно прошли аутентификацию. |
| Соединение завершается по timeout | Увеличьте `timeout` при медленном запуске и `sse_read_timeout` для долгих потоковых ответов. |
| Вызов модели завершается ошибкой | Проверьте `OPENAI_API_KEY` и замените `OpenAIResponses(id=...)` на доступную вашему аккаунту модель. |

## Что дальше

- Используйте [GraphQL-агрегацию](../aggregation.md), чтобы пакетировать
  независимые чтения.
- Ограничьте токен с помощью [scopes для upstream](../authentication.md#scopes-enforced).
- Прочитайте официальное руководство Agno по
  [Streamable HTTP](https://docs.agno.com/tools/mcp/transports/streamable_http).
