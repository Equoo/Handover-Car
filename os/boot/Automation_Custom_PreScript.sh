
echo "37.59.106.4 server" >> /etc/hosts

#/root/setup_network

systemctl start network-keepalive.service

cp /root/setup-network.service /etc/systemd/system/
cp /root/reverse-ssh.service /etc/systemd/system/
cp /root/heartbeat.service /etc/systemd/system/

