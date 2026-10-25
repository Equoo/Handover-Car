#!/bin/bash
# /usr/local/bin/wwan-watchdog.sh

MODEM_ID=2
PING_TARGET="8.8.8.8"

log() {
  logger -t wwan-watchdog "$1"
}

MODEM_JSON=$(mmcli -m "$MODEM_ID" -J 2>/dev/null)
if [ -z "$MODEM_JSON" ]; then
  log "CRITICAL: modem $MODEM_ID unreachable"
  exit 1
fi

MODEM_STATE=$(echo "$MODEM_JSON" | jq -r '.modem.generic.state')

if [ "$MODEM_STATE" != "connected" ]; then
  log "WARN: modem state is '$MODEM_STATE', reconnecting"
  /usr/local/bin/wwan-connect.sh
  exit 0
fi

BEARER_PATH=$(echo "$MODEM_JSON" | jq -r '.modem.generic.bearers[0] // empty')

if [ -z "$BEARER_PATH" ]; then
  log "WARN: no bearer listed, reconnecting"
  /usr/local/bin/wwan-connect.sh
  exit 0
fi

BEARER_ID=$(basename "$BEARER_PATH")
BEARER_JSON=$(mmcli -b "$BEARER_ID" -J 2>/dev/null)
BEARER_CONNECTED=$(echo "$BEARER_JSON" | jq -r '.bearer.status.connected')

if [ "$BEARER_CONNECTED" != "yes" ]; then
  log "WARN: bearer $BEARER_ID not connected, reconnecting"
  /usr/local/bin/wwan-connect.sh
  exit 0
fi

IFACE=$(echo "$BEARER_JSON" | jq -r '.bearer.status.interface')

if ! ip route show table all | grep -q "$IFACE"; then
  log "WARN: no route on $IFACE, reapplying config"
  /usr/local/bin/wwan-connect.sh
  exit 0
fi

if ! ping -c1 -W3 -I "$IFACE" "$PING_TARGET" &>/dev/null; then
  log "WARN: modem+bearer+route OK but ping failed, reconnecting"
  /usr/local/bin/wwan-connect.sh
else
  log "OK: modem=connected bearer=$BEARER_ID iface=$IFACE ping=ok"
fi
