# Источники system prompts (bench adaptations)

Эти файлы **не** являются дословными vendor dumps. Каждый — сокращённая адаптация
для mock harness `query_graphql` (один инструмент, demo dataset). Ведущие строки-комментарии
`#` удаляются `run.py` перед отправкой prompt.

| File | Style | Public sources |
| ---- | ----- | -------------- |
| `system_default.txt` | Minimal bench baseline | Написан для этого repo |
| `system_custom.txt` | Blank alternate slot | Написан для этого repo |
| `system_hermes.txt` | Nous Research Hermes Agent | [prompt assembly docs](https://hermes-agent.nousresearch.com/docs/developer-guide/prompt-assembly); `DEFAULT_AGENT_IDENTITY` / `PARALLEL_TOOL_CALL_GUIDANCE` / `TASK_COMPLETION_GUIDANCE` in [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent) `agent/prompt_builder.py` |
| `system_cursor.txt` | Cursor Agent (pair-programming) | Community dump: [gist sshh12](https://gist.github.com/sshh12/25ad2e40529b269a88b80e7cf1c38084) (March 2025) |
| `system_claude_code.txt` | Claude Code harness | Community dump: [asgeirtj/system_prompts_leaks](https://github.com/asgeirtj/system_prompts_leaks) `Anthropic/Claude Code` (Opus 4.8-era harness section) |

Vendor prompts часто меняются. Перепроверьте linked sources, прежде чем считать
token counts или wording актуальными.
