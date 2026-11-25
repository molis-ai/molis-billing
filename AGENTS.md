# Molis Billing

Billing owns billing, orders, and the Credit ledger. Molis Auth owns identity and organization records. This repository currently contains the Worker infrastructure and operational probes.

- Run commands from the repository root. Use `npm run setup` for locked npm dependencies and Rust 1.96.0 with its WASM target and project-local `worker-build`.
- Run `npm run format:check`, `npm run clippy`, `npm test`, and `npm run build` before submitting changes.
- Keep local development and integration tests on isolated D1 instances. Use Wrangler dry runs to verify packages.
- Keep real secrets in the ignored `secrets/<env>.properties` files. Commit only placeholder templates under `secrets.example/`. Keep generated `.dev.vars*`, Wrangler state, Rust build output, and installed tools out of Git.
- Keep `worker/package-lock.json` and `worker/rust/Cargo.lock` in Git. Run release builds with locked dependencies.
- Handle errors explicitly. Production Rust code must pass Clippy with `unwrap()` denied.
