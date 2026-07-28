Dietpi conf:
DEBIAN 13 Trixie
--Downcloacking: power saving mode (v -2)--
--cpu-governor: cpu save--
safe overcloak

Network:
use setup_network

SSH:
`sudo nano /etc/ssh/sshd_config`

```
GatewayPorts clientspecified
```

setup ssh key
add reverse-ssh.service

Camera driver:
Doc:
<https://spotpear.com/index.php/index/study/detail/id/754.html>

For rpicam needed default imx462

The control scripts needed GPIO CONNECTION and 32bit libs:
sudo dpkg --add-architecture armhf
sudo apt update
sudo apt install libc6:armhf libstdc++6:armhf

PWM SIGNALS:
throtle:  993 (backward) - 1486 1487 1492 - 1986 (forward) in pulses
direction:   1087 - 1480 - 1880

sudo apt install -y modemmanager network-manager usb-modeswitch

remove ifupdown conf in /etc/network/interfaces

echo -e "AT\r" | sudo tee /dev/ttyUSB2
echo -e "AT+QCFG="usbnet",2\r" | sudo tee /dev/ttyUSB2
echo -e "AT+CFUN=1,1\r" | sudo tee /dev/ttyUSB2

sudo systemctl enable --now ModemManager
mmcli -L

-> For checking state

## mmcli -m 0

## journalctl -u ModemManager -f

sudo nmcli connection add type gsm ifname "*" con-name "4g-rm500u" apn "sl2sfr"
sudo nmcli connection modify 4g-rm500u connection.autoconnect yes
sudo nmcli connection up 4g-rm500u

if authentication
sudo nmcli connection modify 4g-rm500u gsm.username "user" gsm.password "pass"

nmcli connection show 4g-rm500u

mmcli -m 0 --signal-setup=5
mmcli -m 0 --signal-get
mmcli -m 0 --location-get

1. Bufferbloat (cake/fq_codel) sur wwan0

Comme discuté, important vu ton adaptive bitrate :

bash
sudo tc qdisc replace dev wwan0 root cake bandwidth 5mbit rtt 100ms

Ajuste bandwidth à ta bitrate cible réelle (un peu en dessous de ton max théorique 4G pour laisser de la marge), et rtt selon ta latence mesurée typique.
