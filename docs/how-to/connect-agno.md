# How to connect an Agno agent to vmcp

**Language:** English | [Русский](../ru/how-to/connect-agno.md)

Connect an Agno agent to vmcp over Streamable HTTP and authenticate it with a
scoped static bearer token.

## Prerequisites

- A vmcp gateway available at a URL such as
  `https://gateway.example.com/mcp`
- Access to the gateway's configured `auth.tokens_file`, or a static token
  supplied by the gateway operator
- Python 3.12 and [`uv`](https://docs.astral.sh/uv/)
- An OpenAI API key for the example agent

## Steps

### 1. Issue a token for Agno

On the vmcp host, create a token with the `mcp:use` scope:

```bash
vmcp pre-reg --name agno --scope mcp:use --out ./tokens.json
```

If this is the first static token, point `auth.tokens_file` at the same file
and restart vmcp once:

```toml
[auth]
tokens_file = "./tokens.json"
```

Copy the printed `vmcp_...` token to the machine that will run Agno. Treat it
as a secret.

### 2. Install Agno with MCP support

Create a virtual environment and install the required packages:

```bash
uv venv --python 3.12
source .venv/bin/activate
uv pip install -U "agno[mcp]" openai
```

### 3. Export the connection settings

Set the gateway URL, vmcp token, and model-provider key:

```bash
export VMCP_URL='https://gateway.example.com/mcp'
export VMCP_TOKEN='vmcp_...'
export OPENAI_API_KEY='sk-...'
```

Do not put these values directly in the Python file.

### 4. Create the Agno agent

Save this example as `agno_vmcp.py`:

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

### 5. Run the agent

Run:

```bash
python agno_vmcp.py
```

The script first confirms that Agno discovered `query_graphql`, then asks the
model to list vmcp's connected upstream servers.

## Verify it worked

- The script does not raise `query_graphql not found`.
- The agent returns a table containing upstream names from `registry.json`.
- The Agno client appears in the vmcp **Sessions** page after its first MCP
  request.

## Troubleshooting

| Problem | Fix |
| ------- | --- |
| The client returns 401 | Confirm `VMCP_TOKEN` matches a token in the active `auth.tokens_file`. |
| The client cannot connect | Check `VMCP_URL`; it must use the Streamable HTTP `/mcp` endpoint, not `/mcp-proxy` or `/admin`. |
| `query_graphql` is missing | Confirm you connected to vmcp itself and that authentication succeeded. |
| The connection times out | Increase `timeout` for slow startup and `sse_read_timeout` for long streamed responses. |
| The model call fails | Confirm `OPENAI_API_KEY` is set and change `OpenAIResponses(id=...)` to a model available to your account. |

## Next steps

- Use [GraphQL aggregation](../aggregation.md) to batch independent reads.
- Restrict the token with [per-upstream scopes](../authentication.md#scopes-enforced).
- Read Agno's official
  [Streamable HTTP transport guide](https://docs.agno.com/tools/mcp/transports/streamable_http).
