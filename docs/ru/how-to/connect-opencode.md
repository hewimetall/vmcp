# Как подключить OpenCode к vmcp

**Язык:** [English](../../how-to/connect-opencode.md) | Русский

Подключите OpenCode к удаленному шлюзу vmcp и завершите OAuth-авторизацию из
CLI OpenCode.

## Предварительные требования

- Шлюз vmcp доступен по публичному HTTPS-адресу
- На шлюзе задано `auth.enabled = true`
- У вас есть мастер-пароль vmcp
- OpenCode установлен локально
- Если `auth.dcr_redirect_uri_allowlist` ограничен, разрешите loopback callback
  OpenCode, добавив `http://127.0.0.1` и `http://localhost`

## Шаги

### 1. Добавьте vmcp в конфигурацию OpenCode

Создайте или обновите `opencode.json` в проекте:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "vmcp": {
      "type": "remote",
      "url": "https://gateway.example.com/mcp",
      "enabled": true,
      "timeout": 30000
    }
  }
}
```

Замените `gateway.example.com` публичным адресом vmcp. URL должен оканчиваться
на `/mcp`.

### 2. Запустите OAuth-авторизацию

Выполните:

```bash
opencode mcp auth vmcp
```

OpenCode обнаружит OAuth endpoints vmcp, выполнит Dynamic Client Registration
(DCR) с Proof Key for Code Exchange (PKCE) и откроет страницу согласия в
браузере.

### 3. Подтвердите подключение

Введите мастер-пароль vmcp на странице `/consent`. Используйте пароль открытым
текстом, а не его Argon2-хеш.

OpenCode сохранит полученные credentials в
`~/.local/share/opencode/mcp-auth.json`.

### 4. Проверьте соединение

Выполните:

```bash
opencode mcp list
```

У записи `vmcp` должен отображаться статус подключения или авторизации.

### 5. Вызовите vmcp из OpenCode

Запустите OpenCode и отправьте:

```text
Use vmcp. Call query_graphql once with:
{ servers { name description toolCount readOnlyCount } }
Return the result as a table.
```

OpenCode должен использовать инструмент vmcp `query_graphql` и вывести
подключенные upstream-серверы.

## Как проверить результат

- `opencode mcp list` показывает **vmcp** подключенным.
- Агент предоставляет инструмент `query_graphql` с префиксом vmcp.
- Тестовый запрос возвращает upstream-имена из `registry.json`.
- После первого MCP-запроса клиент OpenCode появляется на странице vmcp
  **Sessions**.

## Как использовать статический токен

Если вы не хотите запускать OAuth в OpenCode, создайте статический токен по
разделу [Аутентификация](../authentication.md#static-bearer-tokens-pre-reg),
экспортируйте его как `VMCP_TOKEN` и задайте `oauth: false`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "vmcp": {
      "type": "remote",
      "url": "https://gateway.example.com/mcp",
      "enabled": true,
      "oauth": false,
      "headers": {
        "Authorization": "Bearer {env:VMCP_TOKEN}"
      }
    }
  }
}
```

## Устранение неполадок

| Проблема | Решение |
| -------- | ------- |
| Авторизация не запускается | Выполните `opencode mcp auth vmcp` явно. |
| Callback отклонен | Разрешите loopback callback в `auth.dcr_redirect_uri_allowlist` или оставьте allowlist пустым. |
| OpenCode сообщает об ошибке OAuth discovery | Выполните `opencode mcp debug vmcp` и проверьте, что `public_base_url` совпадает с публичным HTTPS-origin. |
| Статический токен возвращает 401 | Проверьте токен в активном `auth.tokens_file` и наличие `VMCP_TOKEN` в окружении процесса OpenCode. |
| Discovery инструментов завершается по timeout | Оставьте `timeout` равным `30000` или увеличьте его при медленном запуске upstream. |

## Что дальше

- Используйте [лестницу discovery для `query_graphql`](../clients.md#использование-graphql-tool).
- Ограничьте клиента с помощью [scopes vmcp](../authentication.md#scopes-enforced).
- Прочитайте официальную [документацию OpenCode по MCP](https://opencode.ai/docs/mcp-servers/).
