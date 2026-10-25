systemctl enable --now reverse-ssh
# systemctl enable heartbeat

apt update

# Rust via rustup, pas via apt : Trixie livre rustc 1.85, qui comprend
# l'edition 2024 mais pas encore les let-chains (stables depuis 1.88) que le
# workspace utilise. L'installeur ajoute lui-meme /root/.cargo/bin au PATH
# dans /root/.profile (ne pas ecrire $PATH ici : envsubst le remplacerait par
# la valeur de la machine qui flashe la carte).
wget -qO- https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable

rm /etc/modprobe.d/dietpi-disable_rpi_camera.conf
rm /etc/modprobe.d/dietpi-disable_rpi_codec.conf
sh -c 'cat > /etc/modules-load.d/rpi-camera.conf <<EOF
bcm2835_isp
bcm2835_v4l2
option
EOF'

apt install -y "linux-headers-$(uname -r)"

wget https://files.waveshare.com/wiki/RM500U-CN/Driver/Linux/RM500U_FOR_RPI.zip
unzip RM500U_FOR_RPI.zip
sudo chmod 777 -R RM500U_FOR_RPI
cd RM500U_FOR_RPI
sudo ./install.sh

sh -c 'echo "2c7c 0900" > /sys/bus/usb-serial/drivers/option1/new_id'
echo 2c7c 0900 >/sys/bus/usb-serial/drivers/generic/new_id

# TODO: replace managed=false to true /etc/NetworkManager/NetworkManager.conf
# TODO: remove tethenrt wifi from /etc/network/interface

nmcli connection add \
  type wifi \
  ifname wlan0 \
  con-name home \
  ssid "MySSID"

nmcli connection modify home \
  wifi-sec.key-mgmt wpa-psk \
  wifi-sec.psk "MyPassword"

nmcli connection modify home connection.autoconnect yes

usermod -aG video,render,audio,pulse-access,dialout dietpi

reboot
