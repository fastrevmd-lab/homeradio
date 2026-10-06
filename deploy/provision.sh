#!/usr/bin/env bash
# Idempotent provisioning for home-radio. Run as root on a Debian 13 host, VM or
# container, from the unpacked bundle directory (deploy/make-bundle.sh builds it).
#
# Bundle layout (files next to this script):
#   radio-web                 release binary                           (required)
#   stations.toml             station list                             (required)
#   config.example.toml       template config                          (required)
#   pipewire/                 50-fallback-sink.conf, raop-sink.conf    (required)
#   systemd/                  cliamp, radio-web, raop-sink units       (required)
#   config.toml               your config                              (optional)
#   caddy/Caddyfile           HTTPS front; installs and enables Caddy  (optional)
#   nftables.conf             firewall; installs and applies nftables  (optional)
# See deploy/caddy/Caddyfile.example and deploy/nftables.conf.example.
#
# /etc/home-radio/config.toml is never overwritten. If neither it nor a bundled
# config.toml exists, the template is installed and this script exits non-zero
# so you can set receiver_url and re-run.
set -euo pipefail
cd "$(dirname "$0")"

CLIAMP_VERSION="v2.3.0"
CLIAMP_SHA256="5c59b5380c5165b473c06148c108ee60a18a1f9961f8013e38b4a68ed5def641"
RADIO_USER="radio"

CONFIG=/etc/home-radio/config.toml

# Fail early, before touching the system, if there is no config to run with.
install -d -m 0755 /etc/home-radio
if [ ! -f "$CONFIG" ]; then
  if [ -f config.toml ]; then
    install -m 0644 config.toml "$CONFIG"
  else
    install -m 0644 config.example.toml "$CONFIG"
    echo "No config found: installed the template at $CONFIG." >&2
    echo "Edit receiver_url (your receiver's address), then re-run provision.sh." >&2
    exit 1
  fi
fi

# The receiver's address, taken from receiver_url in the config: strip quotes,
# scheme, credentials, port and path. Resolves a hostname if it is not an IPv4.
receiver_url="$(sed -n -E "s/^[[:space:]]*receiver_url[[:space:]]*=[[:space:]]*[\"']([^\"']*)[\"'].*/\\1/p" "$CONFIG" | head -n 1)"
if [ -z "$receiver_url" ]; then
  echo "receiver_url not found in $CONFIG - set it to your receiver's URL, e.g. http://192.168.1.50" >&2
  exit 1
fi
receiver_host="$(printf '%s' "$receiver_url" | sed -E 's#^[A-Za-z][A-Za-z0-9+.-]*://##; s#^[^@/]*@##; s#[/:?].*$##')"
if [[ "$receiver_host" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  RECEIVER_IP="$receiver_host"
else
  RECEIVER_IP="$(getent ahostsv4 "$receiver_host" 2>/dev/null | awk 'NR==1{print $1}')"
fi
if [ -z "${RECEIVER_IP:-}" ]; then
  echo "cannot derive an IPv4 address for the receiver from receiver_url '$receiver_url' in $CONFIG" >&2
  exit 1
fi

export DEBIAN_FRONTEND=noninteractive
packages=(pipewire pipewire-bin wireplumber pipewire-alsa libasound2t64
  dbus-user-session ca-certificates curl ffmpeg openssl)
[ -f caddy/Caddyfile ] && packages+=(caddy)
[ -f nftables.conf ] && packages+=(nftables)
apt-get update -qq
apt-get install -y -qq --no-install-recommends "${packages[@]}" >/dev/null

if ! id "$RADIO_USER" >/dev/null 2>&1; then
  useradd --system --create-home --shell /usr/sbin/nologin "$RADIO_USER"
fi
loginctl enable-linger "$RADIO_USER"
RADIO_HOME="$(getent passwd "$RADIO_USER" | cut -d: -f6)"
RADIO_UID="$(id -u "$RADIO_USER")"

# cliamp, pinned and checksum-verified.
if ! /usr/local/bin/cliamp --version 2>/dev/null | grep -q "$CLIAMP_VERSION"; then
  tmp="$(mktemp)"
  curl -fsSL -o "$tmp" "https://github.com/bjarneo/cliamp/releases/download/${CLIAMP_VERSION}/cliamp-linux-amd64"
  echo "${CLIAMP_SHA256}  ${tmp}" | sha256sum -c --quiet
  install -m 0755 "$tmp" /usr/local/bin/cliamp
  rm -f "$tmp"
fi

install -m 0755 radio-web /usr/local/bin/radio-web
install -m 0644 stations.toml /etc/home-radio/stations.toml

# The default cache_dir: holds the cliamp station cache and my-stations.json.
install -d -o "$RADIO_USER" -g "$RADIO_USER" -m 0755 /var/lib/home-radio

install -d -o "$RADIO_USER" -g "$RADIO_USER" -m 0755 \
  "$RADIO_HOME/.config" "$RADIO_HOME/.config/cliamp" "$RADIO_HOME/.cache" \
  "$RADIO_HOME/.cache/home-radio" "$RADIO_HOME/.config/pipewire" \
  "$RADIO_HOME/.config/pipewire/pipewire.conf.d" "$RADIO_HOME/.config/systemd" \
  "$RADIO_HOME/.config/systemd/user"
# The AirPlay sink is NOT a daemon drop-in: it lives in its own on-demand unit
# (raop-sink.service) so radio-web can release and re-handshake the receiver.
rm -f "$RADIO_HOME/.config/pipewire/pipewire.conf.d/50-raop-sink.conf"
install -o "$RADIO_USER" -g "$RADIO_USER" -m 0644 pipewire/50-fallback-sink.conf \
  "$RADIO_HOME/.config/pipewire/pipewire.conf.d/50-fallback-sink.conf"
# raop-sink.conf is a template: render the receiver's address into it.
raop_conf="$(mktemp)"
sed "s|@RECEIVER_IP@|${RECEIVER_IP}|g" pipewire/raop-sink.conf >"$raop_conf"
if grep -q '@RECEIVER_IP@' "$raop_conf"; then
  echo "raop-sink.conf: @RECEIVER_IP@ was not substituted" >&2
  rm -f "$raop_conf"
  exit 1
fi
install -o "$RADIO_USER" -g "$RADIO_USER" -m 0644 "$raop_conf" \
  "$RADIO_HOME/.config/pipewire/raop-sink.conf"
rm -f "$raop_conf"
for unit in cliamp.service radio-web.service raop-sink.service; do
  install -o "$RADIO_USER" -g "$RADIO_USER" -m 0644 "systemd/$unit" "$RADIO_HOME/.config/systemd/user/$unit"
done

# Optional HTTPS front. Certificates are yours to provide; until they exist
# Caddy stays stopped.
if [ -f caddy/Caddyfile ]; then
  install -m 0644 caddy/Caddyfile /etc/caddy/Caddyfile
  install -d -m 0750 -o root -g caddy /etc/caddy/certs
  if [ -r /etc/caddy/certs/fullchain.pem ] && [ -r /etc/caddy/certs/privkey.pem ]; then
    systemctl enable caddy >/dev/null 2>&1
    systemctl restart caddy
  else
    systemctl disable --now caddy >/dev/null 2>&1 || true
    echo "caddy: no certificate yet - place fullchain.pem and privkey.pem in /etc/caddy/certs, then re-run"
  fi
fi

# Optional firewall (replaces the host's whole ruleset).
if [ -f nftables.conf ]; then
  install -m 0644 nftables.conf /etc/nftables.conf
  systemctl enable --now nftables >/dev/null
  nft -f /etc/nftables.conf
fi

# Wait for the user manager that linger starts, then (re)start the stack.
for _ in $(seq 1 20); do
  [ -S "/run/user/${RADIO_UID}/bus" ] && break
  sleep 0.5
done
as_radio() { runuser -u "$RADIO_USER" -- env XDG_RUNTIME_DIR="/run/user/${RADIO_UID}" "$@"; }
as_radio systemctl --user daemon-reload
as_radio systemctl --user enable pipewire.socket pipewire.service wireplumber.service cliamp.service radio-web.service >/dev/null 2>&1
as_radio systemctl --user stop raop-sink.service
as_radio systemctl --user restart pipewire.service wireplumber.service
as_radio systemctl --user restart cliamp.service radio-web.service
echo "provisioned: $(/usr/local/bin/cliamp --version | tail -1), radio-web installed"
