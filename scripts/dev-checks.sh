#!/usr/bin/env bash
set -euo pipefail

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"
export CARGO_PROFILE_TEST_DEBUG="${CARGO_PROFILE_TEST_DEBUG:-0}"
export RUST_TEST_THREADS="${RUST_TEST_THREADS:-2}"
if [[ -n "${CONDA_PREFIX:-}" && -x "${CONDA_PREFIX}/bin/x86_64-conda-linux-gnu-ar" ]]; then
  export AR="${AR:-${CONDA_PREFIX}/bin/x86_64-conda-linux-gnu-ar}"
fi

cargo fmt --all -- --check
cargo check --all-targets --locked
cargo test --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo build --bins --locked
target/debug/clawmingle --help
python3 -B scripts/test_sensitive_data.py
python3 -B scripts/test_qq_smoke.py
python3 -B scripts/test_qq_panels.py
python3 -B scripts/test_clawmingle_admin.py
python3 -B scripts/test_prepare_native_runtime.py
python3 -B scripts/test_native_browser.py
node --test scripts/test_qq_documents_login.mjs
