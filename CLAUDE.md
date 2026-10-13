# higgsfield-mcp

Local Rust MCP server for the pay-as-you-go Higgsfield API. It runs on the user's own computer over
stdio, launched by an MCP client (Claude Code, Claude Desktop). It is not deployed anywhere: there
is no HTTP transport, Dockerfile or hosting config, and none should be added back.

- The Higgsfield key comes from `HF_API_KEY_ID`/`HF_API_KEY_SECRET` in the server's environment.
  Never log or persist it, and only ever send it to `api.higgsfield.ai`.
- Logs go to stderr; stdout is the MCP transport.
- Run `cargo test` before committing. Never call `generate` with real keys in tests: it spends real money.
- After changing code, reinstall with `cargo install --path .` so MCP clients pick up the new binary.
