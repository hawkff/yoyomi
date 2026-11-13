#!/bin/sh
# Checks the workspace: the allowed dependencies between workspace crates
# (ci/layers.txt), formatting, clippy with warnings as errors, the tests and
# cargo-deny. Needs cargo with rustfmt and clippy; installs cargo-deny.
set -eu
cd "$(dirname "$0")/.."

echo "== layers"
status=0
for manifest in crates/*/Cargo.toml; do
	crate=$(sed -n 's/^name = "\(.*\)"$/\1/p' "$manifest" | head -n 1)
	line=$(sed -n "s/^$crate:\(.*\)$/\1/p" ci/layers.txt)
	if [ -z "$(sed -n "/^$crate:/p" ci/layers.txt)" ]; then
		echo "$crate is missing from ci/layers.txt" >&2
		status=1
		continue
	fi
	for dep in $(sed -n 's/^\(yoyomi-[a-z]*\)[ .=].*/\1/p' "$manifest" | sort -u); do
		case " $line " in
		*" $dep "*) ;;
		*)
			echo "$crate must not depend on $dep (see ci/layers.txt)" >&2
			status=1
			;;
		esac
	done
done
[ "$status" -eq 0 ]

echo "== fmt"
cargo fmt --all --check

echo "== clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

echo "== test"
cargo test --workspace --locked

echo "== deny"
command -v cargo-deny >/dev/null || cargo install --locked cargo-deny
cargo deny --locked check
