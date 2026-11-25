# Molis Billing

Molis Billing is the service for billing, orders, and the Credit ledger. Identity and organization records belong to Molis Auth. This initial project provides a Cloudflare Worker in Rust/WASM, a D1 binding, and two operational probes.

## Local development

Node.js 22.18.0 or newer is required. GitHub Actions pins Node.js 24.22.1. Setup installs the locked npm dependencies, Rust 1.96.0 with the WASM target, rustfmt and Clippy, and project-local worker-build 0.8.6.

```sh
npm run setup
npm --prefix worker run db:migrate
npm run dev
```

`npm start -- --env dev` is equivalent. The Worker listens on `127.0.0.1:8788`. The bootstrap migration uses the same local D1 state as the development server and contains a single `SELECT 1` statement.

```sh
curl -i http://127.0.0.1:8788/api/v1/health
curl -i http://127.0.0.1:8788/api/v1/readiness
```

Health reports the Worker identity and environment. Readiness queries D1 and returns HTTP 503 when the query or binding is unavailable.

## Environments

| Environment | Billing origin | Worker | D1 |
| --- | --- | --- | --- |
| `dev` | `http://127.0.0.1:8788` | `molis-billing-local` | `molis-billing-local` |
| `test` | `https://billing.test.molis.ai` | `molis-billing-test` | `molis-billing-test` |
| `prod` | `https://billing.molis.ai` | `molis-billing-production` | `molis-billing-prod` |

`config/<env>.properties` holds non-secret settings and `config/platform-targets.json` records the environment targets. The test and production D1 IDs are placeholders. Dry runs use a fixture ID until each database is configured. Real deployment to either environment requires its own valid D1 ID.

If the Worker gains secrets, store them by environment in ignored `secrets/<env>.properties` files. Copy the matching template from `secrets.example/` and enter one `UPPER_SNAKE_CASE=value` per line. The current Worker requires no secrets.

## Checks

```sh
npm run format:check
npm run clippy
npm test
npm run build
npm run deploy -- --env dev --dry-run
npm run deploy -- --env test --dry-run
npm run deploy -- --env prod --dry-run
```

Use `npm run format` to apply rustfmt and Prettier. The tests cover configuration, Rust routing, and the compiled Worker in Miniflare with in-memory D1. GitHub Actions runs these checks and packages each environment in dry-run mode.
