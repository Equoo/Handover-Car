#! /usr/bin/bash

BOOT_PART=${BOOT_PART:-/dev/sde1}
OS_PART=${OS_PART:-/dev/sde2}

mkdir -p /mnt/boot
mkdir -p /mnt/os

mount $BOOT_PART /mnt/boot
mount $OS_PART /mnt/os

cp -r ./boot/* /mnt/boot/
cp ./boot/Automation_Custom_PreScript.sh /mnt/os/boot/
cp ./boot/Automation_Custom_Script.sh /mnt/os/boot/
cp ./boot/dietpi.txt /mnt/os/boot/
cp -r ./root/* /mnt/os/root
cp -r ./root/.ssh /mnt/os/root

chown -R root:root /root
chmod 700 /mnt/os/root/.ssh
chmod 600 /mnt/os/root/.ssh/id_rsa
chmod 644 /mnt/os/root/.ssh/id_rsa.pub
