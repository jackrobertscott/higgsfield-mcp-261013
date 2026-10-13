# higgsfield-mcp

Rust MCP server for the pay-as-you-go Higgsfield API, deployed on Railway (project
`higgsfield-mcp-261013`, service `higgsfield-mcp`) and auto-deployed from GitHub `master`.

- Commit at regular milestones and push; every push to `master` redeploys.
- Run `cargo test` before committing.
- Never call `generate` with real keys in tests: it spends real money.
- `railway.toml` (Config as Code) is deprecated by Railway and stops working on 2026-12-01; migrate to `.railway/railway.ts` before then.
