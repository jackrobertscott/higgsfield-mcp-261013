# higgsfield-mcp

Public Rust MCP server for the pay-as-you-go Higgsfield API, deployed on Railway (project
`higgsfield-mcp-261013`, service `higgsfield-mcp`) and auto-deployed from GitHub `master`.

- The hosted server is public and multi-tenant: it must never hold Higgsfield credentials or impose
  spend limits. Each caller sends their own key per request (`Authorization: Key ID:SECRET`).
  Never log or persist caller credentials.
- Env-based keys (`HF_API_KEY_ID`/`HF_API_KEY_SECRET`) are for local stdio mode only.
- Commit at regular milestones and push; every push to `master` redeploys.
- Run `cargo test` before committing. Never call `generate` with real keys in tests: it spends real money.
- `railway.toml` (Config as Code) is deprecated by Railway and stops working on 2026-12-01; migrate to `.railway/railway.ts` before then.
