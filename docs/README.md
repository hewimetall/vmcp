# Документация vmcp

Руководство оператора по развертыванию и эксплуатации виртуального MCP gateway.

| Документ | Что описывает |
| -------- | ------------- |
| [deployment.md](deployment.md) | Развертывание через GHCR: Compose + Caddy, bare metal, TLS, переменные окружения, чеклист |
| [authentication.md](authentication.md) | Поток OAuth 2.1, master password, static tokens, dev-режим без auth, диагностика |
| [builds-and-modes.md](builds-and-modes.md) | Cargo features, release-бинарники, HTTP gateway, опциональный `[tasks]` |
| [upstreams.md](upstreams.md) | Регистрация upstream-сервисов, tools (sidecar + lock) и prompts |
| [tasks.md](tasks.md) | Нативные MCP Tasks (`run_task`), SQLite store, allowlist, поток SEP-1686 |
| [sessions.md](sessions.md) | Реестр admin sessions и записей (JSON в `sessions_dir`) |
| [skills.md](skills.md) | YAML skill playbooks → MCP `prompts/list` / `prompts/get` |
| [clients.md](clients.md) | Cursor HTTP+OAuth, скриптовые MCP/HTTP-клиенты, vmcp-lite для локального stdio host |
| [bench.md](bench.md) | Опциональный Python-инструмент: замер пакетирования LLM-запросов `query_graphql` |
| [aggregation.md](aggregation.md) | Как работает GraphQL-агрегация поверх upstream tools |

Быстрые ссылки из корня репозитория:

- Шаблон конфигурации: [`vmcp.toml`](../vmcp.toml)
- Docker-стек: [`deploy/bootstrap.sh`](../deploy/bootstrap.sh) + [`docker-compose.yml`](../docker-compose.yml) + [`deploy/Caddyfile`](../deploy/Caddyfile) (image из GHCR / workflow `release`)
- Демо: [`demo/README.md`](../demo/README.md) + [`demo/vmcp.toml`](../demo/vmcp.toml)
