# Development shortcuts. CI runs the same commands (.github/workflows/ci.yml).

.PHONY: check test lint fmt coverage gui-smoke site

check: lint test

test:
	cargo test --workspace --locked
	python3 -m unittest discover -s scripts/tests

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	shellcheck get.sh install.sh packaging/*/*.sh packaging/appimage/AppRun

fmt:
	cargo fmt --all

# Needs cargo-llvm-cov (cargo install cargo-llvm-cov).
coverage:
	cargo llvm-cov --workspace --locked

# Needs Xvfb, xwininfo and dbus-run-session.
gui-smoke:
	cargo build -p unpakt-gui --locked
	dbus-run-session -- python3 scripts/gui_smoke.py target/debug/unpakt-gui

# Needs mkdocs and mkdocs-material; serves the site on http://127.0.0.1:8000.
site:
	python3 scripts/build_site.py
	mkdocs serve
