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

There is default but need last rpi kernel:
```
sudo apt update
sudo apt full-upgrade
sudo apt install raspberrypi-kernel raspberrypi-kernel-headers
```

The control scripts needed 32bit libs:
sudo dpkg --add-architecture armhf
sudo apt update
sudo apt install libc6:armhf libstdc++6:armhf


but to test the officials:
https://github.com/veyeimaging/raspberrypi_v4l2

