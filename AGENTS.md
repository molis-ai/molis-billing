# Molis Billing

Billing owns billing, orders, and the Credit ledger. Molis Auth owns identity and organization records. The Rust/WASM Worker implements subscriptions, payment events, refunds, credit reservations and settlement, audit pagination, and scheduled expiry. Initialize isolated D1 from `worker/migrations/0001_billing_baseline.sql`.

- Run commands from the repository root. Use `npm run setup` for locked npm dependencies and Rust 1.96.0 with its WASM target and project-local `worker-build`.
- Run `npm run format:check`, `npm run clippy`, `npm test`, and `npm run build` before submitting changes.
- Clippy covers default, no-default-features and test-fixture builds. Use `npm --prefix worker run build:rust -- --production` to verify the deployment artifact.
- Keep local development and integration tests on isolated D1 instances. Use Wrangler dry runs to verify packages.
- Keep real secrets in the ignored `secrets/<env>.properties` files. Commit only placeholder templates under `secrets.example/`. Keep generated `.dev.vars*`, Wrangler state, Rust build output, and installed tools out of Git.
- Keep `worker/package-lock.json` and `worker/rust/Cargo.lock` in Git. Run release builds with locked dependencies.
- Handle errors explicitly. Production Rust code must pass Clippy with `unwrap()` denied.
