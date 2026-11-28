# Molis Billing

Molis Billing is the service for billing, orders, and the Credit ledger. Identity and organization records belong to Molis Auth. This project provides a Cloudflare Worker in Rust/WASM, a scoped catalog, subscriptions, payments and refunds, credit holds and settlements, paginated audit history, and operational probes. The [Billing v1 contract](docs/billing-v1-contract.md) describes the Auth transport and shared interfaces.

## Local development

Node.js 22.18.0 or newer is required. GitHub Actions pins Node.js 24.22.1. Setup installs the locked npm dependencies, Rust 1.96.0 with the WASM target, rustfmt and Clippy, and project-local worker-build 0.8.6.

```sh
npm run setup
npm --prefix worker run db:migrate
npm run dev
```

`npm start -- --env dev` is equivalent. The Worker listens on `127.0.0.1:8788`. Initialize a fresh database with `worker/migrations/0001_billing_baseline.sql`. The local migration command uses the development server's D1 state and seeds two Work test plans: personal (CNY 10/month or 100/year, 100 credits/month) and team (CNY 30/month or 300/year, 500 credits/month). Running it again preserves existing plan versions. The test data lives outside the shared migrations.

```sh
curl -i http://127.0.0.1:8788/api/v1/health
curl -i http://127.0.0.1:8788/api/v1/readiness
```

Health reports the Worker identity and environment. Readiness checks that the billing tables are readable and returns HTTP 503 if the binding or migrations are missing. An empty database is ready after migration.

## Environments

| Environment | Billing origin | Worker | D1 |
| --- | --- | --- | --- |
| `dev` | `http://127.0.0.1:8788` | `molis-billing-local` | `molis-billing-local` |
| `test` | `https://billing.test.molis.ai` | `molis-billing-test` | `molis-billing-test` |
| `prod` | `https://billing.molis.ai` | `molis-billing-production` | `molis-billing-prod` |

`config/<env>.properties` holds non-secret settings, including the separate D1 IDs for test and production. `config/platform-targets.json` records the environment targets. Apply the migrations in `worker/migrations/` to the corresponding D1 database before deploying a new environment.

Store `MOLIS_INTERNAL_SECRET` in the ignored `secrets/<env>.properties` file, using the matching template from `secrets.example/`. Use the same 32-byte base64url secret in Auth and Billing for that environment. Dev connects to Auth on loopback. Test and prod connect to the matching `AuthInternal` Service Binding. Health and readiness work without the secret.

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

Use `npm run format` to apply rustfmt and Prettier. The tests cover configuration, calendar boundaries, the compiled Worker and named bindings in Miniflare, and isolated D1 baseline initialization and repeatable seeds. Clippy checks the default, production, and test-fixture feature sets. Use `npm --prefix worker run build:rust -- --production` to package the Worker without the mock payment adapter or test routes. The prod deployment command and its dry-run both build with `--no-default-features`. GitHub Actions runs these checks and packages each environment in dry-run mode.
