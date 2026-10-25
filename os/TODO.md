USE AUTOSSH to do reverse ssh
do not push ssh keys

add Driver sudo modprobe snd-usb-audio

insall pulseaudio alsa-utils

 sudo tee /etc/systemd/system/pulseaudio.service << 'EOF'
  [Unit]
  Description=PulseAudio Sound Server
  After=sound.target

  [Service]
  Type=simple
  ExecStart=/usr/bin/pulseaudio --system --disallow-exit --disallow-module-loading=0
  Restart=on-failure

  [Install]
  WantedBy=multi-user.target
  EOF

amixer -c 0 set Mic 100%

important because default is 40% very low
