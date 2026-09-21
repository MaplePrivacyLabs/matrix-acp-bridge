# Contributor guidance

Keep Matrix and ACP provider choices configurable. Examples use reserved domains and fictional accounts. Never commit credentials, deployment configuration, device/session IDs, private messages, or operator-specific paths.

Use the pinned Rust toolchain and lockfile. Run `cargo fmt --check`, `cargo clippy --locked --all-features --all-targets -- -D warnings`, and `cargo test --locked --all-features` for code changes. Offline fixtures must not contact Matrix or a real model. Report live interoperability separately from fixture coverage.

Preserve fail-closed sender, audience, encryption, permission-mode and session-binding checks. Environment filtering and ACP permission prompts are not OS isolation. Do not start a live bridge or agent merely to run tests; live operations require the operator's authorization and isolated credentials.

Live tests must use explicitly authorized test identities and rooms. Do not issue sessions for real human accounts or send messages as a person without explicit permission to use that identity. Server administration access is not consent to impersonate a user.
