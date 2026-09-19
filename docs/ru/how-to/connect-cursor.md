# Как подключить Cursor к vmcp

**Язык:** [English](../../how-to/connect-cursor.md) | Русский

Подключите Cursor к развернутому шлюзу vmcp по Streamable HTTP и авторизуйте
его через OAuth vmcp.

## Предварительные требования

- Шлюз vmcp доступен по публичному HTTPS-адресу
- На шлюзе задано `auth.enabled = true`
- У вас есть мастер-пароль vmcp
- Cursor поддерживает MCP
- Если `auth.dcr_redirect_uri_allowlist` ограничен, разрешите используемый
  callback Cursor:
  - Desktop: `http://localhost:8787/callback`
  - Web и Cursor Agents: `https://www.cursor.com/agents/mcp/oauth/callback`

## Шаги

### 1. Создайте конфигурацию MCP для Cursor

Создайте `.cursor/mcp.json` в проекте или `~/.cursor/mcp.json` для глобальной
конфигурации:

```json
{
  "mcpServers": {
    "vmcp": {
      "url": "https://gateway.example.com/mcp"
    }
  }
}
```

Замените `gateway.example.com` публичным адресом vmcp. URL должен оканчиваться
на `/mcp`.

### 2. Включите сервер в Cursor

Перезапустите Cursor, откройте **Customize > MCPs** и включите **vmcp**. Cursor
подключится к endpoint, обнаружит метаданные OAuth и откроет авторизацию в
браузере.

### 3. Авторизуйте Cursor

Введите мастер-пароль vmcp на странице `/consent`. Используйте пароль открытым
текстом, а не его Argon2-хеш.

После подтверждения Cursor сохранит OAuth-клиента и access token. vmcp хранит
DCR `client_id` в `auth.clients_db_path`; после рестарта шлюза access token
может потребовать обновления.

### 4. Вызовите vmcp из чата

Отправьте в Cursor:

```text
Use the vmcp query_graphql tool once with this document:
{ servers { name description toolCount readOnlyCount } }
Return the result as a table.
```

Cursor должен один раз вызвать `query_graphql` и вывести подключенные к vmcp
upstream-серверы.

## Как проверить результат

- **vmcp** отображается подключенным в **Customize > MCPs**.
- Cursor показывает `query_graphql` среди доступных инструментов сервера.
- Тестовый запрос возвращает имена из вашего `registry.json`.
- После первого MCP-запроса клиент Cursor появляется на странице vmcp
  **Sessions**.

## Как использовать статический токен

Если OAuth через браузер недоступен, создайте статический токен на хосте шлюза
и настройте `auth.tokens_file` по разделу
[Аутентификация](../authentication.md#static-bearer-tokens-pre-reg). Перед
запуском Cursor экспортируйте токен:

```bash
export VMCP_TOKEN='vmcp_...'
```

Добавьте заголовок авторизации:

```json
{
  "mcpServers": {
    "vmcp": {
      "url": "https://gateway.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${env:VMCP_TOKEN}"
      }
    }
  }
}
```

Не коммитьте токен. Проектный `.cursor/mcp.json` может храниться в общем
репозитории.

## Устранение неполадок

| Проблема | Решение |
| -------- | ------- |
| Cursor не подключается к vmcp | Откройте `https://gateway.example.com/health` и убедитесь, что ответ равен `ok`. |
| OAuth открывается на неправильном хосте | Значения `public_base_url` и `auth.issuer` должны совпадать с публичным HTTPS-origin. |
| DCR отклоняет callback | Добавьте активный callback Cursor в `auth.dcr_redirect_uri_allowlist` или оставьте allowlist пустым. |
| Статический токен возвращает 401 | Проверьте, что `auth.tokens_file` указывает на файл с токеном, а Cursor запущен с `VMCP_TOKEN` в окружении. |
| Сервер подключен, но инструмента нет | Откройте **Output**, выберите **MCP Logs** и проверьте, что URL оканчивается на `/mcp`. |

## Что дальше

- Используйте [лестницу discovery для `query_graphql`](../clients.md#использование-graphql-tool).
- Настройте [scopes и доступ к upstream](../authentication.md#scopes-enforced).
- Прочитайте официальную [документацию Cursor по MCP](https://cursor.com/docs/mcp).
