#!/usr/bin/env bash
set -euo pipefail

PORT="/dev/ttyUSB0"
BAUD="115200"

# Configure port: raw, 115200, no echo
stty -F "$PORT" "$BAUD" raw -echo -hupcl

# Open device once on FD 3 for writing
exec 3>"$PORT"

STEER=108
THR=98

# optional: send an initial newline to flush parser
printf "%d %d\n" "$STEER" "$THR" >&3

while true; do
    printf "%d %d\n" "$STEER" "$THR" >&3
    # flush to make sure data is sent immediately
    # note: >&3 is line-buffered here; if you want strict flushing use: fflush via Python/perl or use tcdrain
    sleep 0.05   # 20 Hz
done

# If you ever want to close FD3 in the script:
# exec 3>&-

