#!/bin/bash
# /usr/local/bin/modem-disable-sleep.sh

AT_PORT="/dev/ttyUSB2"

# Attendre que le port existe (le modem peut mettre du temps à s'énumérer au boot)
for i in $(seq 1 30); do
  [ -e "$AT_PORT" ] && break
  sleep 1
done

if [ ! -e "$AT_PORT" ]; then
  logger -t modem-sleep "AT port $AT_PORT not found after 30s, aborting"
  exit 1
fi

# Configurer le port en mode raw pour éviter les problèmes de terminal
stty -F "$AT_PORT" raw -echo

# Envoyer la commande
echo -e "AT+QSCLK=0\r" >"$AT_PORT"
sleep 1

logger -t modem-sleep "Sent AT+QSCLK=0 to $AT_PORT"
