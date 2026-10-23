#!/bin/bash
# Codec cost per video frame on one CPU thread, the way WebRTC's software codecs run in a
# browser: libvpx VP8 and VP9 and libaom AV1 in real-time mode, at Ferrite's top simulcast
# bitrate for each step (see bitrateFor in web/js/views/talk-call.js), plus decoding.
# Measured as CPU time (user + system) per frame, so other load on the machine barely
# affects it. The source is ffmpeg's testsrc2 pattern with light temporal noise (roughly a
# webcam's sensor noise); the generator's own cost, measured with a pass that only produces
# raw frames, is subtracted.
#
# Encoder speeds follow WebRTC's desktop defaults (VP8 6; VP9 7, and 9 from 1080p up; AV1
# 10) as far as ffmpeg allows: it caps VP9 and AV1 at speed 8, so from 1080p up VP9, and AV1
# at every size, cost a little less in a browser than shown here.
#
#   bash docs/bench/codec-cost.sh [FRAMES]        (needs ffmpeg with libvpx, libaom, dav1d)
set -euo pipefail
FRAMES=${1:-120}
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
cpu() { # runs ffmpeg with -benchmark; prints its user+sys CPU seconds
  ffmpeg -benchmark "$@" 2>&1 >/dev/null | sed -n 's/.*bench: utime=\([0-9.]*\)s stime=\([0-9.]*\)s.*/\1 \2/p' | awk '{print $1+$2}'
}
src() { echo "testsrc2=size=$1:rate=$2,noise=alls=6:allf=t,format=yuv420p"; }
kbps() { # height fps -> top-layer bitrate in kbit/s, same formula as the web client
  awk -v h="$1" -v f="$2" 'BEGIN{bpp=0.055*(720/h)^0.25; printf "%d", h*h*16/9*f*bpp/1000}'
}
printf "%-9s %-6s %8s %10s %10s %11s %11s\n" step codec kbit/s enc_ms enc_fps/core dec_ms dec_fps/core
for step in "1280x720 30" "1920x1080 30" "1920x1080 60" "3840x2160 30" "3840x2160 60"; do
  set -- $step; size=$1; fps=$2; h=${size#*x}
  rate=$(kbps "$h" "$fps")
  base=$(cpu -nostdin -f lavfi -i "$(src "$size" "$fps")" -frames:v "$FRAMES" -threads 1 -f null -)
  for codec in vp8 vp9 av1; do
    case $codec in
      vp8) enc=(-c:v libvpx -deadline realtime -cpu-used 6 -error-resilient 1 -lag-in-frames 0 -auto-alt-ref 0); ext=webm ;;
      vp9) enc=(-c:v libvpx-vp9 -deadline realtime -cpu-used $(( h >= 1080 ? 8 : 7 )) -row-mt 0 -lag-in-frames 0 -error-resilient 1); ext=webm ;;
      av1) enc=(-c:v libaom-av1 -usage realtime -cpu-used 8 -row-mt 0 -lag-in-frames 0); ext=webm ;;
    esac
    out="$TMP/$codec.$ext"
    t=$(cpu -nostdin -y -f lavfi -i "$(src "$size" "$fps")" -frames:v "$FRAMES" -threads 1 "${enc[@]}" \
          -b:v "${rate}k" -maxrate "${rate}k" -minrate "${rate}k" -g 3000 -an "$out")
    case $codec in vp8) dec=(-c:v vp8) ;; vp9) dec=(-c:v vp9) ;; av1) dec=(-c:v libdav1d) ;; esac
    d=$(cpu -nostdin -threads 1 "${dec[@]}" -i "$out" -threads 1 -f null -)
    awk -v s="${h}p$fps" -v c="$codec" -v r="$rate" -v t="$t" -v b="$base" -v d="$d" -v n="$FRAMES" 'BEGIN{
      e=(t-b)*1000/n; if (e<0.01) e=0.01; dd=d*1000/n; if (dd<0.01) dd=0.01;
      printf "%-9s %-6s %8d %10.1f %10.0f %11.1f %11.0f\n", s, c, r, e, 1000/e, dd, 1000/dd }'
  done
done
