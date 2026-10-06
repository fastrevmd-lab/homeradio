#!/usr/bin/env bash
# Build radio-web and stage a deployable bundle (see the layout in provision.sh).
# Usage: deploy/make-bundle.sh [DIR]   (default: target/bundle)
# Copy DIR to a Debian 13 host and run provision.sh there as root. Optional
# files (config.toml, caddy/Caddyfile, nftables.conf) can be added to DIR
# before it is shipped.
set -euo pipefail
cd "$(dirname "$0")/.."

bundle="${1:-target/bundle}"

cargo build --release

rm -rf "$bundle"
mkdir -p "$bundle"
install -m 0755 target/release/radio-web "$bundle/radio-web"
install -m 0644 config/stations.toml "$bundle/stations.toml"
install -m 0644 config/config.example.toml "$bundle/config.example.toml"
cp -r deploy/pipewire "$bundle/pipewire"
cp -r deploy/systemd "$bundle/systemd"
install -m 0755 deploy/provision.sh "$bundle/provision.sh"

echo "bundle ready: $(cd "$bundle" && pwd)"
