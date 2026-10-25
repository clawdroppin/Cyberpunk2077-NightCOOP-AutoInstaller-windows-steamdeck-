#!/usr/bin/env bash
# NightCoop hub — one-command setup for Ubuntu 24.04+ / Debian 13+ (a VPS, a Linux PC, or WSL on Windows).
#
# Follows the official "without Docker" guide: https://github.com/Twenfys/nightcoop-hub (tools/hub/docker/README.md)
#
#   curl -fsSL https://raw.githubusercontent.com/clawdroppin/Cyberpunk2077-NightCOOP-AutoInstaller-windows-steamdeck-/main/scripts/nightcoop-hub-setup.sh | sudo bash
#
# Running it again updates the hub to the newest official source and keeps the key (same fingerprint).
#
# Options (after `bash -s --` when piping):
#   --shards N        hub threads (default: auto = every CPU core; ports 47810 … 47810+N-1)
#   --port P          entry port (default 47800)
#   --name NAME       hub name shown in players' logs (default: this machine's name)
#   --region R        region label (default: home)
#   --no-service      build and install only; don't create/start a systemd service (WSL without systemd)
#   --uninstall       stop and remove the hub (keeps a copy of the key in /root/nightcoop-hub-sign.key.bak)
set -euo pipefail

PORT=47800
SHARDS=auto
NAME="$(hostname 2>/dev/null || echo nightcoop-hub)"
REGION=home
SERVICE=1
UNINSTALL=0
HUB_REPO="https://github.com/Twenfys/nightcoop-hub.git"
ENET_REPO="https://github.com/lsalzman/enet.git"
# The ENet commit the official guide pins (read again from the guide below, this is only the fallback).
ENET_COMMIT_FALLBACK="5a9c537fd464b3c6d3c55e1d3bd47588faf71b42"
SRC=/opt/nightcoop-hub
DATA=/var/lib/nightcoop
KEY="$DATA/hub-sign.key"
BIN=/usr/local/bin/nightcoop_hub

while [ $# -gt 0 ]; do
  case "$1" in
    --shards) SHARDS="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --name) NAME="$2"; shift 2 ;;
    --region) REGION="$2"; shift 2 ;;
    --no-service) SERVICE=0; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    *) echo "Unknown option: $1" >&2; exit 2 ;;
  esac
done

say() { printf '\n\033[1;33m==> %s\033[0m\n' "$*"; }
die() { printf '\n\033[1;31mERROR: %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "Run as root (put 'sudo' in front, or log in as root)."
command -v apt-get >/dev/null || die "This script needs Ubuntu 24.04+ or Debian 13+ (apt). Pick that image for your server."

if [ "$UNINSTALL" -eq 1 ]; then
  say "Removing the NightCoop hub"
  systemctl disable --now nightcoop-hub 2>/dev/null || true
  pkill -x nightcoop_hub 2>/dev/null || true
  [ -f "$KEY" ] && cp -p "$KEY" /root/nightcoop-hub-sign.key.bak && echo "Key saved to /root/nightcoop-hub-sign.key.bak"
  rm -f /etc/systemd/system/nightcoop-hub.service "$BIN" /etc/sysctl.d/90-nightcoop-hub.conf
  systemctl daemon-reload 2>/dev/null || true
  rm -rf "$SRC"
  echo "Done. The data folder $DATA (with the key) was left in place; delete it yourself if you want a new fingerprint."
  exit 0
fi

say "Installing build tools"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq g++ gcc git ca-certificates curl >/dev/null
GXX_MAJOR="$(g++ -dumpversion | cut -d. -f1)"
[ "$GXX_MAJOR" -ge 13 ] || die "g++ $GXX_MAJOR is too old (the hub needs C++20, g++ 13+). Use Ubuntu 24.04 or Debian 13."

say "Getting the official hub source"
if [ -d "$SRC/.git" ]; then
  git -C "$SRC" fetch -q --depth 1 origin
  git -C "$SRC" reset -q --hard FETCH_HEAD
else
  rm -rf "$SRC"
  git clone -q --depth 1 "$HUB_REPO" "$SRC"
fi
echo "Hub source: $(git -C "$SRC" log -1 --format='%h %cs %s')"

# Use the ENet commit the guide currently names, so a future guide update is followed automatically.
ENET_COMMIT="$(grep -ho 'checkout [0-9a-f]\{40\}' "$SRC/tools/hub/docker/README.md" 2>/dev/null | head -1 | cut -d' ' -f2 || true)"
ENET_COMMIT="${ENET_COMMIT:-$ENET_COMMIT_FALLBACK}"
if [ ! -d "$SRC/.enet/.git" ]; then
  git clone -q "$ENET_REPO" "$SRC/.enet"
fi
git -C "$SRC/.enet" fetch -q origin
git -C "$SRC/.enet" checkout -q "$ENET_COMMIT"

say "Building (a minute or two)"
cd "$SRC"
rm -rf obj && mkdir -p obj
for f in callbacks compress host list packet peer protocol unix; do
  gcc -c -O2 -I.enet/include -DHAS_FCNTL=1 -DHAS_POLL=1 -DHAS_SOCKLEN_T=1 -DHAS_MSGHDR_FLAGS=1 -DHAS_GETADDRINFO=1 \
      -DHAS_GETNAMEINFO=1 -DHAS_INET_PTON=1 -DHAS_INET_NTOP=1 -DHAS_GETHOSTBYNAME_R=1 -DHAS_GETHOSTBYADDR_R=1 \
      ".enet/$f.c" -o "obj/enet_$f.o"
done
gcc -c -O2 vendor/monocypher/monocypher.c -o obj/monocypher.o
for f in src/Log.cpp src/NetCrypto.cpp src/HubServer.cpp tools/hub/main.cpp; do
  g++ -c -std=c++20 -O2 -I.enet/include "$f" -o "obj/$(basename "$f" .cpp).o"
done
g++ -o nightcoop_hub obj/*.o -pthread

say "Installing"
WAS_RUNNING=0
if systemctl is-active --quiet nightcoop-hub 2>/dev/null; then WAS_RUNNING=1; systemctl stop nightcoop-hub; fi
pkill -x nightcoop_hub 2>/dev/null && sleep 1 || true
install -m 755 nightcoop_hub "$BIN"
id nightcoop >/dev/null 2>&1 || useradd --system --create-home --home-dir "$DATA" --shell /usr/sbin/nologin nightcoop
mkdir -p "$DATA" && chown nightcoop: "$DATA" && chmod 700 "$DATA"

# A bigger receive buffer, as the guide recommends (the hub asks for 4 MB).
echo 'net.core.rmem_max = 8388608' > /etc/sysctl.d/90-nightcoop-hub.conf
sysctl -q -p /etc/sysctl.d/90-nightcoop-hub.conf 2>/dev/null || sysctl -q -w net.core.rmem_max=8388608 2>/dev/null || true

if [ "$SHARDS" = "auto" ]; then THREADS="$(nproc)"; else THREADS="$SHARDS"; fi
LAST_PORT=$((PORT + 10 + THREADS - 1))

# Open the ports in ufw when it is active (the cloud provider's firewall is separate — see the helper's guide).
if command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q "Status: active"; then
  ufw allow "$PORT/udp" >/dev/null
  ufw allow "$((PORT + 10)):$LAST_PORT/udp" >/dev/null
  echo "ufw: opened UDP $PORT and $((PORT + 10))-$LAST_PORT"
fi

ARGS="$PORT --shards $SHARDS --max-clients 512 --sign-key-auto $KEY --hub-name $NAME --region $REGION"
if [ "$SERVICE" -eq 1 ] && [ -d /run/systemd/system ]; then
  cat > /etc/systemd/system/nightcoop-hub.service <<EOF
[Unit]
Description=NightCoop hub
After=network-online.target
Wants=network-online.target

[Service]
User=nightcoop
ExecStart=$BIN $ARGS
Restart=always
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=$DATA
ProtectHome=true
PrivateTmp=true
MemoryMax=1G

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable --now nightcoop-hub >/dev/null 2>&1
  [ "$WAS_RUNNING" -eq 1 ] && echo "Restarted the hub on the new build."
  # The hub creates its key on the first start.
  for _ in $(seq 1 20); do [ -s "$KEY" ] && break; sleep 0.5; done
else
  echo "Not creating a service (--no-service or no systemd). Start it with:"
  echo "  sudo -u nightcoop $BIN $ARGS"
  if [ ! -s "$KEY" ]; then
    # Start once briefly so the key (and so the fingerprint) exists.
    sudo -u nightcoop "$BIN" $ARGS >/dev/null 2>&1 &
    for _ in $(seq 1 20); do [ -s "$KEY" ] && break; sleep 0.5; done
    kill $! 2>/dev/null || true
  fi
fi

[ -s "$KEY" ] || die "The hub didn't create its key. Check: journalctl -u nightcoop-hub (or run the command above by hand)."
FINGERPRINT="$(sudo -u nightcoop "$BIN" --hub-public "$KEY" 2>&1 | grep -oE '[A-Z0-9]{4}(-[A-Z0-9]{4}){3}' | head -1 || true)"
PUBLIC_IP="$(curl -4 -fsS --max-time 5 https://api.ipify.org 2>/dev/null || curl -4 -fsS --max-time 5 https://ifconfig.me 2>/dev/null || echo YOUR-SERVER-IP)"

say "Done"
echo "Open these UDP ports in your provider's firewall (and router, at home): $PORT and $((PORT + 10))-$LAST_PORT"
echo "Back up the key file $KEY — it is your server's identity (same key = same fingerprint)."
echo
echo "NIGHTCOOP_HUB_ADDRESS=${PUBLIC_IP}#${FINGERPRINT}"
echo
echo "Paste that address into NightCoop Helper → Host your own server, or in the game: Co-op → Server: … → Add an own server."
