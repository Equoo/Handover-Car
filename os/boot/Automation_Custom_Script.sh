systemctl enable --now reverse-ssh
# systemctl enable --now heartbeat
# systemctl enable network_keepalive

apt update
# apt install -y openssh-client rpicam-apps netcat-openbsd make cargo libdbus-1-dev pkg-config unzip minicom

rm /etc/modprobe.d/dietpi-disable_rpi_camera.conf
rm /etc/modprobe.d/dietpi-disable_rpi_codec.conf
sh -c 'cat > /etc/modules-load.d/rpi-camera.conf <<EOF
bcm2835_isp
bcm2835_v4l2
option
EOF'

# echo "auto usb0
# iface usb0 inet dhcp
# " > /etc/network/interfaces
#
# sh -c 'echo "2c7c 0900" > /sys/bus/usb-serial/drivers/option1/new_id'
# echo 2c7c 0900 > /sys/bus/usb-serial/drivers/generic/new_id

apt install -y "linux-headers-$(uname -r)"

wget https://files.waveshare.com/wiki/RM500U-CN/Driver/Linux/RM500U_FOR_RPI.zip
unzip RM500U_FOR_RPI.zip
sudo chmod 777 -R RM500U_FOR_RPI
cd RM500U_FOR_RPI
sudo ./install.sh

reboot
