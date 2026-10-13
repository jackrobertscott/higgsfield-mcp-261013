# higgsfield-mcp

A local Rust [MCP](https://modelcontextprotocol.io) server for the **pay-as-you-go Higgsfield API**
(`api.higgsfield.ai`), for use from Claude Code and Claude Desktop. It uses API keys from
[cloud.higgsfield.ai](https://cloud.higgsfield.ai) and bills your API balance in real money, not
the credits included with a higgsfield.ai subscription.

It runs on your own computer over stdio: your MCP client starts it as a subprocess, your key stays
on your machine and is only sent to Higgsfield, and it can read input files from and save outputs
to your disk.

The model catalog is read from the live [Higgsfield docs](https://docs.higgsfield.ai), so new
models appear without an update.

## Tools

| Tool | Cost | What it does |
| - | - | - |
| `list_models` | free | Model families and workflows, with `endpoint` and `doc_path` |
| `get_model_docs` | free | Input schema, allowed values and output shape for a workflow |
| `estimate_cost` | free | USD and credit estimate for an endpoint and params |
| `generate` | **paid** | Submits a generation, billed to your API balance |
| `get_request_status` / `wait_for_request` | free | Poll a request and return output URLs |
| `cancel_request` | refund | Cancel a request that is still queued |
| `upload_file` | free | Upload media from a local file, a public https URL or base64 and get a `public_url` |
| `download_output` | free | Save a generated output to your computer (outputs expire after ~7 days) |

## Install

Requires [Rust](https://rustup.rs). From this folder:

```bash
cargo install --path .
```

This puts `higgsfield-mcp` in `~/.cargo/bin`. Run the same command again after pulling changes.

Create an API key at [cloud.higgsfield.ai](https://cloud.higgsfield.ai) and export it in your
shell profile (e.g. `~/.zshrc`):

```bash
export HF_API_KEY_ID="..."
export HF_API_KEY_SECRET="..."
```

Without a key the server still starts, and `list_models` and `get_model_docs` work.

## Connecting

### Claude Code

Claude Code passes your shell environment to the server, so the key comes from your shell profile:

```bash
claude mcp add --scope user higgsfield -- ~/.cargo/bin/higgsfield-mcp
```

### Claude Desktop

Claude Desktop doesn't load your shell profile, so set the key in `claude_desktop_config.json`
and use the full path to the binary:

```json
{
  "mcpServers": {
    "higgsfield": {
      "command": "/Users/YOU/.cargo/bin/higgsfield-mcp",
      "env": { "HF_API_KEY_ID": "...", "HF_API_KEY_SECRET": "..." }
    }
  }
}
```

## Configuration

| Variable | Default | Notes |
| - | - | - |
| `HF_API_KEY_ID` / `HF_API_KEY_SECRET` | | Your Higgsfield API key |
| `HF_OUTPUT_DIR` | `~/Downloads/higgsfield` | Where `download_output` saves files when no `dir` is given |
| `RUST_LOG` | `info` | Log filter. Logs go to stderr |

## Development

```bash
cargo test
cargo run          # speaks MCP on stdin/stdout
```
