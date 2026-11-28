# Molis Billing

Billing owns billing, orders, and the Credit ledger. Molis Auth owns identity and organization records. The Rust/WASM Worker implements subscriptions, payment events, refunds, credit reservations and settlement, audit pagination, and scheduled expiry. Initialize isolated D1 from `worker/migrations/0001_billing_baseline.sql`.

- Run commands from the repository root. Use `npm run setup` for locked npm dependencies and Rust 1.96.0 with its WASM target and project-local `worker-build`.
- Run `npm run format:check`, `npm run clippy`, `npm test`, and `npm run build` before submitting changes.
- Clippy covers default, no-default-features and test-fixture builds. Use `npm --prefix worker run build:rust -- --production` to verify the deployment artifact.
- Keep local development and integration tests on isolated D1 instances. Use Wrangler dry runs to verify packages.
- Keep real secrets in the ignored `secrets/<env>.properties` files. Commit only placeholder templates under `secrets.example/`. Keep generated `.dev.vars*`, Wrangler state, Rust build output, and installed tools out of Git.
- Keep `worker/package-lock.json` and `worker/rust/Cargo.lock` in Git. Run release builds with locked dependencies.
## Rust module standards

- Keep every `mod.rs` at 25 lines or fewer. Use it only for module documentation, child declarations, and re-exports. Put implementation, data types, SQL, and HTTP handlers in named files.
- Use Rust's standard module directory layout. Do not use `#[path = "..."]`.
- Use explicit imports in production code. Wildcard imports are reserved for inline unit-test modules.
- Order the top of each file as module documentation, standard library and third-party imports, crate and parent imports, constants and type aliases, then types and implementations. Keep imports out of function bodies.
- Handle errors explicitly and avoid lint exemptions. The crate denies Clippy's `unwrap_used`, `expect_used`, `panic`, and `wildcard_imports` lints in production code.
- Use the narrowest private or restricted visibility for internal code. Reserve unrestricted `pub` for the Worker entry points in `lib.rs`. Keep feature-gated test routes and fixtures under `testing/`.
