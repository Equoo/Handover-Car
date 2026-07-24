#! /usr/bin/bash

BOOT_PART=${BOOT_PART:-/dev/sde1}
OS_PART=${OS_PART:-/dev/sde2}

mkdir -p /mnt/boot
mkdir -p /mnt/os

mount $BOOT_PART /mnt/boot
mount $OS_PART /mnt/os

set -a
source .env
set +a

work_dir=$(mktemp -d)

for file in ./boot/*; do
  [ -f "$file" ] || continue
  filename=$(basename "$file")
  envsubst <"$file" >"$work_dir/${filename}"
done

cp -r $work_dir/* /mnt/boot/
cp -r $work_dir/* /mnt/os/boot/
cp -r ./root/* /mnt/os/root
cp -r ./root/.ssh /mnt/os/root

rm -Rf $work_dir

chown -R root:root /root
chmod 700 /mnt/os/root/.ssh
chmod 600 /mnt/os/root/.ssh/id_rsa
chmod 644 /mnt/os/root/.ssh/id_rsa.pub
