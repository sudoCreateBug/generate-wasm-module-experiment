#!/usr/bin/env bash
# Builds both components, plugs the inner module into the generated wrapper,
# and calls every wrapped function.
set -euo pipefail
cd "$(dirname "$0")"

cargo build --target wasm32-wasip2 --release -p inner -p wrapper
mkdir -p out
target=target/wasm32-wasip2/release
wac plug "$target/wrapper.wasm" --plug "$target/inner.wasm" -o out/composed.wasm

# Only the example packages; the rest is WASI pulled in by Rust's std.
example_wit() { wasm-tools component wit "$1" | sed -n '/^package example:.* {$/,/^}$/p'; }

echo "--- wrapper.wasm (imports the original, exports the wrapped interface)"
example_wit "$target/wrapper.wasm"

echo "--- composed.wasm (inner plugged into wrapper)"
wasm-tools component wit out/composed.wasm | grep -E "^\s*(import|export) example:"

echo "--- calls"
for call in 'motto()' 'greet("Alice")' 'loaves-for(21)'; do
    printf '%-18s => ' "$call"
    wasmtime run --invoke "example:greeter-wrapped/greeter.$call" out/composed.wasm
done
