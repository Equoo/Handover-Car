systemctl enable --now reverse-ssh
systemctl enable --now heartbeat
systemctl enable setup-network

apt update
apt install -y openssh-client rpicam-apps

rm /etc/modprobe.d/dietpi-disable_rpi_camera.conf
rm /etc/modprobe.d/dietpi-disable_rpi_codec.conf
sh -c 'cat > /etc/modules-load.d/rpi-camera.conf <<EOF
bcm2835_isp
bcm2835_v4l2
EOF'

reboot
