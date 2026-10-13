# higgsfield-mcp

A Rust [MCP](https://modelcontextprotocol.io) server for the **pay-as-you-go Higgsfield API**
(`api.higgsfield.ai`), for use from Claude Code and Claude Desktop. It uses API keys from
[cloud.higgsfield.ai](https://cloud.higgsfield.ai) and bills your API balance in real money, not
the credits included with a higgsfield.ai subscription.

The model catalog is read from the live [Higgsfield docs](https://docs.higgsfield.ai), so new
models appear without a redeploy.

## Tools

| Tool | Cost | What it does |
| - | - | - |
| `list_models` | free | Model families and workflows, with `endpoint` and `doc_path` |
| `get_model_docs` | free | Input schema, allowed values and output shape for a workflow |
| `estimate_cost` | free | USD and credit estimate for an endpoint and params |
| `generate` | **paid** | Submits a generation, billed to your API balance |
| `get_request_status` / `wait_for_request` | free | Poll a request and return output URLs |
| `cancel_request` | refund | Cancel a request that is still queued |
| `upload_file` | free | Upload media from a public https URL or base64 and get a `public_url` |

## Public server, your own key

The hosted server is public and holds no Higgsfield credentials. Each client sends its own key
with every request, read from the machine it runs on:

```
Authorization: Key <KEY_ID>:<KEY_SECRET>
```

This is the same header format Higgsfield uses. The server also accepts `Bearer <KEY_ID>:<KEY_SECRET>`,
or the `X-Higgsfield-Key-Id` and `X-Higgsfield-Key-Secret` header pair. The server forwards the key to
`api.higgsfield.ai` for that one call and never stores or logs it. Every call bills the caller's own
Higgsfield account, and the server sets no spending limits.

If you don't want your key to pass through any server you don't run, use [local stdio mode](#local-stdio-mode).

Public endpoint: `https://higgsfield-mcp-production-4c73.up.railway.app/mcp`

## Connecting

Create an API key at [cloud.higgsfield.ai](https://cloud.higgsfield.ai) and export it in your shell
profile on your own computer:

```bash
export HF_API_KEY_ID="..."
export HF_API_KEY_SECRET="..."
```

### Claude Code

```bash
claude mcp add --scope user --transport http higgsfield https://higgsfield-mcp-production-4c73.up.railway.app/mcp --header "Authorization: Key ${HF_API_KEY_ID}:${HF_API_KEY_SECRET}"
```

### Claude Desktop

Custom connectors in Claude Desktop can't send headers, so use the `mcp-remote` bridge. It runs on
your computer and adds the header for you. In `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "higgsfield": {
      "command": "npx",
      "args": [
        "-y", "mcp-remote",
        "https://higgsfield-mcp-production-4c73.up.railway.app/mcp",
        "--header", "Authorization:${HF_AUTH}"
      ],
      "env": { "HF_AUTH": "Key YOUR_KEY_ID:YOUR_KEY_SECRET" }
    }
  }
}
```

### Local stdio mode

Run the binary on your own machine so your key never leaves it except to reach Higgsfield:

```bash
cargo install --git https://github.com/jackrobertscott/higgsfield-mcp-261013
```

```json
{
  "mcpServers": {
    "higgsfield": {
      "command": "higgsfield-mcp",
      "args": ["--stdio"],
      "env": { "HF_API_KEY_ID": "...", "HF_API_KEY_SECRET": "..." }
    }
  }
}
```

For Claude Code: `claude mcp add --scope user higgsfield -e HF_API_KEY_ID=$HF_API_KEY_ID -e HF_API_KEY_SECRET=$HF_API_KEY_SECRET -- higgsfield-mcp --stdio`

## Server configuration

| Variable | Default | Notes |
| - | - | - |
| `PORT` | | When set, the server runs in public HTTP mode; otherwise it uses stdio |
| `MCP_ALLOWED_HOSTS` | any | Optional comma-separated `Host` allowlist |
| `HF_API_KEY_ID` / `HF_API_KEY_SECRET` | | **stdio mode only.** Ignored in HTTP mode |

## Deployment (Railway)

The Railway project `higgsfield-mcp-261013` builds from this repo's `Dockerfile` and redeploys on
every push to `master`. Don't set Higgsfield keys on the server.

## Development

```bash
cargo test
PORT=8080 cargo run
```
