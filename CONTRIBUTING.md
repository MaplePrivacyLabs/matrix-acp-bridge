# Contributing

Keep the bridge provider-neutral and deployment-neutral. Examples use reserved domains, fictional accounts and configurable paths. Do not commit a real deployment config, secret-manager project ID, token, device/session ID, private conversation, or private Git history.

Before a code change is ready:

```sh
cargo fmt --check
cargo clippy --locked --all-features --all-targets -- -D warnings
cargo test --locked --all-features
```

Add regression tests for admission, credential handling, session continuity or transport changes. Use the in-memory ACP fixture for protocol tests. Offline tests must not connect to a live server/model. Do not weaken checks to make an integration pass; describe the actual incompatible capability instead.

For adapter compatibility reports, include its version, arguments, mode and sanitized observations of new work, resumed follow-up, denial and cancellation. Distinguish a successful `doctor` from a real Matrix exchange. Use disposable accounts and scoped credentials for live testing.

Useful next contributions: additional ACP adapter coverage, packaged binaries, simpler provider authentication, safe per-thread workspaces, longer-gap/key recovery, clearer local failure diagnostics, and live approval/cancellation tests. Keep changes small enough to review and avoid turning company-specific provisioning into a required dependency.
