set dotenv-load := false

ci:
	cargo run -p xtask -- ci

fmt:
	cargo run -p xtask -- fmt

clippy:
	cargo run -p xtask -- clippy

test:
	cargo run -p xtask -- test

bench:
	cargo run -p xtask -- bench

cov:
	cargo run -p xtask -- cov

doc:
	cargo run -p xtask -- doc

publish:
	cargo run -p xtask -- publish

run:
	cargo run -- --
