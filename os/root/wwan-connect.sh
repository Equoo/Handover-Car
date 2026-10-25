#!/bin/bash
# /usr/local/bin/wwan-connect.sh

MODEM_ID=2
APN="sl2sfr"
IP_TYPE="ipv4v6"

log() {
  logger -t wwan-connect "$1"
}

MODEM_JSON=$(mmcli -m "$MODEM_ID" -J 2>/dev/null)
if [ -z "$MODEM_JSON" ]; then
  log "ERROR: modem $MODEM_ID unreachable"
  exit 1
fi

EXISTING_BEARERS=$(echo "$MODEM_JSON" | jq -r '.modem.generic.bearers[]?')

FOUND_CONNECTED=""
for BEARER_PATH in $EXISTING_BEARERS; do
  BEARER_ID=$(basename "$BEARER_PATH")
  BEARER_JSON=$(mmcli -b "$BEARER_ID" -J 2>/dev/null)
  CONNECTED=$(echo "$BEARER_JSON" | jq -r '.bearer.status.connected')
  BEARER_APN=$(echo "$BEARER_JSON" | jq -r '.bearer.properties.apn')

  if [ "$CONNECTED" == "yes" ] && [ "$BEARER_APN" == "$APN" ]; then
    FOUND_CONNECTED="$BEARER_ID"
    break
  fi
done

if [ -n "$FOUND_CONNECTED" ]; then
  log "Bearer $FOUND_CONNECTED already connected with apn=$APN, reusing"
  BEARER_ID="$FOUND_CONNECTED"
else
  # Nettoyer les bearers déconnectés qui traînent
  for BEARER_PATH in $EXISTING_BEARERS; do
    BID=$(basename "$BEARER_PATH")
    BJSON=$(mmcli -b "$BID" -J 2>/dev/null)
    CONNECTED=$(echo "$BJSON" | jq -r '.bearer.status.connected')
    if [ "$CONNECTED" != "yes" ]; then
      log "Deleting stale bearer $BID"
      mmcli -m "$MODEM_ID" --delete-bearer="$BID" 2>/dev/null
    fi
  done

  log "Creating new bearer apn=$APN ip-type=$IP_TYPE"
  CREATE_OUT=$(mmcli -m "$MODEM_ID" --create-bearer="apn=$APN,ip-type=$IP_TYPE" 2>&1)
  BEARER_ID=$(echo "$CREATE_OUT" | grep -oP '/Bearer/\K[0-9]+')

  if [ -z "$BEARER_ID" ]; then
    log "ERROR: bearer creation failed: $CREATE_OUT"
    exit 1
  fi

  log "Connecting bearer $BEARER_ID"
  mmcli -m "$MODEM_ID" --bearer="$BEARER_ID" --connect
  sleep 2
fi

BEARER_JSON=$(mmcli -b "$BEARER_ID" -J 2>/dev/null)
CONNECTED=$(echo "$BEARER_JSON" | jq -r '.bearer.status.connected')

if [ "$CONNECTED" != "yes" ]; then
  log "ERROR: bearer $BEARER_ID still not connected"
  exit 1
fi

IFACE=$(echo "$BEARER_JSON" | jq -r '.bearer.status.interface')
IP_ADDR=$(echo "$BEARER_JSON" | jq -r '.bearer["ipv4-config"].address')
IP_PREFIX=$(echo "$BEARER_JSON" | jq -r '.bearer["ipv4-config"].prefix')
GATEWAY=$(echo "$BEARER_JSON" | jq -r '.bearer["ipv4-config"].gateway')
DNS1=$(echo "$BEARER_JSON" | jq -r '.bearer["ipv4-config"].dns[0] // empty')
DNS2=$(echo "$BEARER_JSON" | jq -r '.bearer["ipv4-config"].dns[1] // empty')

if [ -z "$IP_ADDR" ] || [ "$IP_ADDR" == "null" ]; then
  log "ERROR: no IPv4 config on bearer $BEARER_ID"
  exit 1
fi

log "Applying IP=$IP_ADDR/$IP_PREFIX gateway=$GATEWAY dns=$DNS1,$DNS2 iface=$IFACE"

ip link set "$IFACE" up

if [ -f "/sys/class/net/$IFACE/qmi/raw_ip" ]; then
  echo Y >"/sys/class/net/$IFACE/qmi/raw_ip" 2>/dev/null
fi

ip addr flush dev "$IFACE"
ip addr add "$IP_ADDR/$IP_PREFIX" dev "$IFACE"
ip route replace default via "$GATEWAY" dev "$IFACE"

{
  [ -n "$DNS1" ] && echo "nameserver $DNS1"
  [ -n "$DNS2" ] && echo "nameserver $DNS2"
} >/etc/resolv.conf

log "wwan-connect finished: $IP_ADDR via $GATEWAY on $IFACE"
