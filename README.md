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
https://spotpear.com/index.php/index/study/detail/id/754.html

For rpicam needed default imx462

The control scripts needed GPIO CONNECTION and 32bit libs:
sudo dpkg --add-architecture armhf
sudo apt update
sudo apt install libc6:armhf libstdc++6:armhf


PWM SIGNALS:
throtle:  993 (backward) - 1486 1487 1492 - 1986 (forward) in pulses
direction:   1087 - 1480 - 1880
