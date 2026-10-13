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
| `generate` | **paid** | Estimates, checks the spend caps, then submits. Needs `max_usd` |
| `get_request_status` / `wait_for_request` | free | Poll a request and return output URLs |
| `cancel_request` | refund | Cancel a request that is still queued |
| `upload_file` | free | Upload media from a public https URL or base64 and get a `public_url` |
| `get_spend_limits` | free | Show the caps and today's estimated spend |

## Spending safeguards

- `generate` always calls the estimate endpoint first and refuses when the estimate is above
  the caller's `max_usd`, `HF_MAX_USD_PER_REQUEST`, or the remaining `HF_MAX_USD_PER_DAY` budget.
- If a cost can't be estimated, nothing is submitted (unless `HF_ALLOW_UNESTIMATED=true`).
- Every submission sends an `Idempotency-Key`, so a retry after a network failure is not charged twice.
- The daily total lives in memory and resets when the server restarts. Your real balance is on
  [cloud.higgsfield.ai](https://cloud.higgsfield.ai).

## Configuration

| Variable | Required | Default | Notes |
| - | - | - | - |
| `HF_API_KEY_ID` | yes | | From cloud.higgsfield.ai |
| `HF_API_KEY_SECRET` | yes | | |
| `MCP_AUTH_TOKEN` | HTTP mode | | 24+ characters. Clients send it as a Bearer token or in the path `/mcp/<token>` |
| `HF_MAX_USD_PER_REQUEST` | no | `5` | |
| `HF_MAX_USD_PER_DAY` | no | unlimited | |
| `HF_ALLOW_UNESTIMATED` | no | `false` | |
| `MCP_ALLOWED_HOSTS` | no | any | Comma-separated `Host` allowlist |
| `PORT` | no | | When set, the server runs in HTTP mode; otherwise it uses stdio |

## Deployment (Railway)

The Railway project `higgsfield-mcp-261013` builds from this repo's `Dockerfile` and redeploys on
every push to `master`. The MCP endpoint is `https://<domain>/mcp`.

```bash
railway variables --service higgsfield-mcp --set "HF_API_KEY_ID=..." --set "HF_API_KEY_SECRET=..."
```

## Connecting

Read the token without printing it into your shell history:

```bash
TOKEN=$(railway variables --service higgsfield-mcp --kv | grep '^MCP_AUTH_TOKEN=' | cut -d= -f2-)
```

**Claude Code**

```bash
claude mcp add --scope user --transport http higgsfield https://higgsfield-mcp-production-4c73.up.railway.app/mcp --header "Authorization: Bearer $TOKEN"
```

**Claude Desktop / claude.ai**: Settings → Connectors → Add custom connector, and use this URL
(the token goes in the path because custom connectors can't send headers):

```
https://higgsfield-mcp-production-4c73.up.railway.app/mcp/<MCP_AUTH_TOKEN>
```

Treat that URL as a secret. To revoke access, change `MCP_AUTH_TOKEN` in Railway.

**Local stdio** (no Railway needed):

```json
{
  "mcpServers": {
    "higgsfield": {
      "command": "/path/to/higgsfield-mcp",
      "args": ["--stdio"],
      "env": { "HF_API_KEY_ID": "...", "HF_API_KEY_SECRET": "...", "HF_MAX_USD_PER_REQUEST": "5" }
    }
  }
}
```

## Development

```bash
cargo test
HF_API_KEY_ID=... HF_API_KEY_SECRET=... MCP_AUTH_TOKEN=$(openssl rand -hex 32) PORT=8080 cargo run
```
