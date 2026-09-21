default:
    @just --list

check:
    cargo fmt --check
    cargo clippy --locked --all-features --all-targets -- -D warnings
    cargo test --locked --all-features

demo scenario="conversation":
    cargo run --locked -- demo {{scenario}}

config-check path="config/example.toml":
    cargo run --locked -- check --config {{path}}
