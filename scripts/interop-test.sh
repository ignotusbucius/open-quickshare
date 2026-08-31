#!/usr/bin/env bash
# =============================================================================
# interop-test.sh — guided Quick Share interop test matrix for open-quickshare
#
#   Payloads : short text · long text · image · big mp4
#   Direction: SEND (PC -> phone)  and  RECEIVE (phone -> PC)
#   Transport: same-LAN Wi-Fi · Wi-Fi Direct (phone hosts) · pure BLE
#
# It drives the REAL transfer code through the headless examples
# (tx_send / rx_service, same core_lib paths the app uses), so no GUI is
# involved and nothing can crash a webview mid-test. You only touch the phone
# when prompted. Each cell reports PASS/FAIL and the transport that was
# ACTUALLY used, then a summary matrix is printed and saved.
#
# Usage:
#   scripts/interop-test.sh                 # full matrix, interactive
#   scripts/interop-test.sh --send-only
#   scripts/interop-test.sh --recv-only
#   scripts/interop-test.sh --transport lan|direct|ble   # just one transport
#   TARGET_NAME="Pixel" scripts/interop-test.sh          # filter the phone
#
# Nothing here hard-codes a device name, MAC or account — you supply the phone
# filter at runtime (or via TARGET_NAME); leave it blank to match any receiver.
# =============================================================================
set -uo pipefail

# ---- config ----------------------------------------------------------------
REPO="${REPO:-$HOME/.local/src/rquickshare-ble}"
CONTAINER="${CONTAINER:-rqs-build}"
APPIMAGE="${APPIMAGE:-$HOME/AppImages/openquickshare.appimage}"
EX_DIR="$REPO/core_lib/target/release/examples"
WORK="${WORK:-$HOME/.cache/oqs-interop}"
PAYLOAD_DIR="$WORK/payloads"
DL_DIR="$WORK/received"                       # rx_service download target
REPORT="$WORK/report-$(date +%Y%m%d-%H%M%S).txt"
RECV_TIMEOUT="${RECV_TIMEOUT:-60}"           # seconds to wait for a phone->PC file
SEND_SCAN="${SEND_SCAN:-1}"                   # tx_send does its own 6x20s scan

BLU=$'\e[1;34m'; GRN=$'\e[1;32m'; RED=$'\e[1;31m'; YEL=$'\e[1;33m'; DIM=$'\e[2m'; RST=$'\e[0m'
say()  { printf '%s\n' "$*"; }
head1(){ printf '\n%s========== %s ==========%s\n' "$BLU" "$*" "$RST"; }
ask()  { printf '%s%s%s ' "$YEL" "$*" "$RST"; }
ok()   { printf '%s  ✔ %s%s\n' "$GRN" "$*" "$RST"; }
bad()  { printf '%s  ✗ %s%s\n' "$RED" "$*" "$RST"; }

# ---- selection flags -------------------------------------------------------
DO_SEND=1; DO_RECV=1; ONLY_TRANSPORT=""
while [ $# -gt 0 ]; do
  case "$1" in
    --send-only) DO_RECV=0 ;;
    --recv-only) DO_SEND=0 ;;
    --transport) shift; ONLY_TRANSPORT="${1:-}" ;;
    -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown arg: $1"; exit 2 ;;
  esac
  shift
done

mkdir -p "$PAYLOAD_DIR" "$DL_DIR"
: > "$REPORT"
RESULTS="$WORK/results.psv"; : > "$RESULTS"   # dir|transport|payload|status|medium|note

# ---- phone filter ----------------------------------------------------------
if [ -z "${TARGET_NAME:-}" ]; then
  echo
  ask "Phone name filter (substring of your phone's Quick Share name; blank = any receiver):"
  read -r TARGET_NAME
fi
export TARGET_NAME
say "${DIM}Using phone filter: '${TARGET_NAME:-<any>}'${RST}"

# ---- locate / verify example binaries --------------------------------------
need_build=0
for b in tx_send rx_service; do [ -x "$EX_DIR/$b" ] || need_build=1; done
if [ "$need_build" = 1 ]; then
  head1 "Building test binaries (tx_send, rx_service)"
  distrobox enter --name "$CONTAINER" -- bash -lc \
    "cd '$REPO/core_lib' && cargo build --release --features experimental --example tx_send --example rx_service" \
    || { bad "example build failed"; exit 1; }
fi
for b in tx_send rx_service; do
  [ -x "$EX_DIR/$b" ] || { bad "missing $EX_DIR/$b"; exit 1; }
done
ok "test binaries ready"

# ---- sample payloads -------------------------------------------------------
hsize() { numfmt --to=iec --suffix=B --format='%.1f' "$(stat -c%s "$1")" 2>/dev/null || stat -c%s "$1"; }

gen_payloads() {
  head1 "Preparing sample payloads"
  local st="$PAYLOAD_DIR/short.txt" lt="$PAYLOAD_DIR/long.txt"
  local im="$PAYLOAD_DIR/image.jpg" mp="$PAYLOAD_DIR/big.mp4"

  # short: a single line (~90 B -> one BLE frame)
  printf 'open-quickshare interop test — short clipboard text · %s · ünïçø∂é 🚀\n' "$(date)" > "$st"

  # long: ~64 KB of text (multi-chunk over BLE, still < 1 MB cap)
  if [ ! -s "$lt" ] || [ "$(stat -c%s "$lt")" -lt 40000 ]; then
    : > "$lt"; local i=0
    while [ "$(stat -c%s "$lt")" -lt 65536 ]; do
      printf '%04d  The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs. 0123456789.\n' "$i" >> "$lt"
      i=$((i+1))
    done
  fi

  # image: realistic sub-MB JPEG (multi-chunk over BLE, under the 1 MB cap)
  if [ ! -s "$im" ] || [ "$(stat -c%s "$im")" -gt 1000000 ]; then
    if command -v magick >/dev/null;  then magick  -size 1280x960 plasma:fractal -quality 80 "$im"
    elif command -v convert >/dev/null; then convert -size 1280x960 plasma:fractal -quality 80 "$im"
    else head -c 350000 /dev/urandom > "$im"; fi
  fi

  # big mp4: > 1 MB on purpose so it must upgrade to Wi-Fi (and is refused on pure BLE).
  # testsrc compresses tiny, so force a high bitrate to land ~20 MB.
  if [ ! -s "$mp" ] || [ "$(stat -c%s "$mp")" -lt 5000000 ]; then
    if command -v ffmpeg >/dev/null; then
      ffmpeg -y -loglevel error -f lavfi -i "mandelbrot=size=1920x1080:rate=30" \
             -f lavfi -i "sine=frequency=440:duration=30" -t 30 \
             -c:v libx264 -pix_fmt yuv420p -b:v 5M -maxrate 5M -bufsize 10M \
             -c:a aac -shortest "$mp" \
        || head -c 20000000 /dev/urandom > "$mp"
    else head -c 20000000 /dev/urandom > "$mp"; fi
  fi

  printf '  %-6s %8s  (%s)\n' \
    short "$(hsize "$st")" "single BLE frame" \
    long  "$(hsize "$lt")" "multi-chunk BLE" \
    image "$(hsize "$im")" "multi-chunk BLE, < 1 MB" \
    mp4   "$(hsize "$mp")" "> 1 MB -> forces Wi-Fi; refused on pure BLE"
}

# payload registry: label|path
payload_path() {
  case "$1" in
    short) echo "$PAYLOAD_DIR/short.txt" ;;
    long)  echo "$PAYLOAD_DIR/long.txt"  ;;
    image) echo "$PAYLOAD_DIR/image.jpg" ;;
    mp4)   echo "$PAYLOAD_DIR/big.mp4"   ;;
  esac
}
PAYLOADS=(short long image mp4)

# ---- app process management (comm-based; never pattern-matches this script) -
app_pids() { pgrep -x rquickshare 2>/dev/null; pgrep -x openquickshare. 2>/dev/null; }
stop_app() {
  local p; local any=0
  for p in $(app_pids); do any=1; kill "$p" 2>/dev/null; done
  [ "$any" = 1 ] && sleep 2
  for p in $(app_pids); do kill -9 "$p" 2>/dev/null; done
  [ -n "$(app_pids)" ] && sleep 1
}
start_app() {
  [ -x "$APPIMAGE" ] || { say "${DIM}(no AppImage at $APPIMAGE to relaunch)${RST}"; return; }
  setsid -f bash -c "exec '$APPIMAGE' >/dev/null 2>&1"
}

# ---- transport detection from a run log ------------------------------------
detect_medium() {
  local log="$1"
  # Order matters: match markers of the transport ACTUALLY used, not medium
  # names merely listed in BWU offer/retry frames.
  if   grep -qiE 'too large to send over Bluetooth' "$log"; then echo "REFUSED-too-big"
  elif grep -qE 'upgraded to Wi-Fi-LAN; payload continues over TCP' "$log"; then echo "Wi-Fi LAN"
  elif grep -qE 'upgraded to.*(hotspot|Wi-Fi Direct)|joined.*(hotspot|group)' "$log"; then echo "Wi-Fi Direct"
  elif grep -qE "hosting hotspot .* for the sender to join|Hotspot: 'DIRECT-.*' up on" "$log"; then echo "Wi-Fi Direct (we host)"
  elif grep -qE 'phone connected over TCP' "$log"; then echo "Wi-Fi LAN"
  elif grep -qE 'UPGRADE_FAILURE; continuing over BLE|stays on BLE|BLE only, no Wi-Fi upgrade' "$log"; then echo "BLE"
  elif grep -qE 'SendingFiles|ReceivingFiles' "$log"; then echo "BLE"
  else echo "?"; fi
}

record() { printf '%s|%s|%s|%s|%s|%s\n' "$1" "$2" "$3" "$4" "$5" "$6" >> "$RESULTS"; }

# ---- one SEND cell ---------------------------------------------------------
run_send() {
  local transport="$1" mediums="$2" label="$3"
  local file; file="$(payload_path "$label")"
  local sz; sz="$(hsize "$file")"
  local log="$WORK/send-$transport-$label.log"
  head1 "SEND · $transport · $label ($sz)"
  say "Phone: make sure it is on the Quick Share ${YEL}receive screen (Everyone)${RST}, then it will show an Accept prompt."
  ask "[Enter]=run   s=skip:"; read -r a; [ "$a" = s ] && { record SEND "$transport" "$label" SKIP - "skipped"; return; }

  # TRUE app parity (manager.rs): payloads ≤ 1 MB advertise BLE_L2CAP only
  # ([10]); larger payloads advertise the block's Wi-Fi set. Advertising Wi-Fi
  # mediums on a small send is something the app never does — and the phone
  # can hang up right after paired-key when we do.
  local eff_mediums="$mediums"
  [ "$(stat -c%s "$file")" -le 1048576 ] && eff_mediums="10"
  say "${DIM}  mediums: [$eff_mediums]${RST}"

  # The headless tx_send uses a single-shot BLE dial (no retry ladder like the
  # app), and the phone's L2CAP PSM rotates, so a fresh scan+dial can miss.
  # Retry the whole send a couple times on a dial miss before giving up.
  local rc=1 attempt
  for attempt in 1 2 3; do
    PACKET_SEND_MEDIUMS="$eff_mediums" RUST_LOG="info,rqs_lib=debug,mdns_sd=error" \
      "$EX_DIR/tx_send" "$file" >"$log" 2>&1
    rc=$?
    grep -q 'final state Finished' "$log" && break
    grep -qiE 'too large to send over Bluetooth' "$log" && break
    grep -qiE "couldn't establish a live L2CAP|no receiver found" "$log" || break
    [ "$attempt" -lt 3 ] && { say "  ${DIM}BLE dial miss (attempt $attempt/3); re-scanning…${RST}"; sleep 3; }
  done
  local med; med="$(detect_medium "$log")"

  if grep -q 'final state Finished' "$log"; then
    ok "PC reports Finished  (medium: $med)"
    ask "Did the file actually arrive/appear on the phone? [y/N]:"; read -r got
    if [[ "$got" =~ ^[Yy] ]]; then record SEND "$transport" "$label" PASS "$med" "confirmed on phone"
    else                          record SEND "$transport" "$label" FAIL "$med" "PC ok but phone did NOT receive"; fi
  elif [ "$med" = "REFUSED-too-big" ]; then
    ok "Refused up front as too large for BLE (expected on pure BLE)"
    record SEND "$transport" "$label" "EXPECT-REFUSE" "$med" "correctly refused >1MB on BLE"
  else
    bad "send failed (rc=$rc). tail:"; tail -4 "$log" | sed 's/^/    /'
    record SEND "$transport" "$label" FAIL "$med" "tx_send rc=$rc; see $log"
  fi
}

# ---- one RECEIVE cell ------------------------------------------------------
run_receive() {
  local transport="$1" label="$2"
  local log="$WORK/recv-$transport-$label.log"
  head1 "RECEIVE · $transport · $label"
  rm -f "$DL_DIR"/* 2>/dev/null
  # plain background (no setsid) so kill/wait actually reach rx_service
  RQS_DOWNLOAD_DIR="$DL_DIR" RQS_DEVICE_NAME="OQS Interop RX" \
    RUST_LOG="info,rqs_lib=debug,mdns_sd=error" \
    "$EX_DIR/rx_service" >"$log" 2>&1 &
  local rxpid=$!
  sleep 3
  say "Phone: Quick Share → ${YEL}Send${RST} → pick the $label → choose ${YEL}'OQS Interop RX'${RST}."
  ask "[Enter] once you've STARTED the send on the phone   (s=skip):"; read -r a; echo
  if [ "$a" = s ]; then kill -INT "$rxpid" 2>/dev/null; wait "$rxpid" 2>/dev/null; record RECV "$transport" "$label" SKIP - "skipped"; return; fi

  say "  Waiting for the transfer — press ${YEL}Enter${RST} anytime to stop waiting early."
  # PASS requires the receiver to log completion AND the bytes on disk to stop
  # growing. A merely-existing file is an IN-PROGRESS transfer — judging (and
  # killing the receiver) on file existence aborts it mid-flight.
  local waited=0 idle=0 got="" verdict="" note="" last_sz=-1 fin_at=-1
  while :; do
    local sz; sz=$(du -sb "$DL_DIR" 2>/dev/null | cut -f1); sz=${sz:-0}
    if [ "$sz" != "$last_sz" ]; then last_sz="$sz"; idle=0; else idle=$((idle+2)); fi
    if grep -qE 'Transfer finished|TEXT RECEIVED' "$log"; then
      [ "$fin_at" -lt 0 ] && fin_at="$waited"
      # finished: let writes settle, then judge
      if [ "$idle" -ge 4 ] || [ $((waited - fin_at)) -ge 10 ]; then
        got="$(find "$DL_DIR" -type f -size +0c 2>/dev/null | head -1)"
        if [ -n "$got" ]; then verdict=PASS
        else verdict=NOFILE; note="receiver reported finished but wrote NO file"; fi
        break
      fi
    fi
    # timeout counts only IDLE seconds — an actively-growing file resets it
    [ "$idle" -ge "$RECV_TIMEOUT" ] && { verdict=TIMEOUT; break; }
    if read -t 2 -N 1 -r _k 2>/dev/null; then verdict=STOP; break; fi
    waited=$((waited+2)); printf '\r  …%ss (%s bytes on disk)   ' "$waited" "$sz"
  done
  echo
  local med; med="$(detect_medium "$log")"
  kill -INT "$rxpid" 2>/dev/null; wait "$rxpid" 2>/dev/null
  # If a hosted-hotspot cell got killed hard, its DIRECT-* connection can
  # linger and hijack later cells' IPs — tear any leftovers down.
  local c
  for c in $(nmcli -t -f NAME connection show --active 2>/dev/null | grep -i '^DIRECT-'); do
    nmcli connection down "$c" >/dev/null 2>&1
  done

  if [ "$verdict" = PASS ] && [ -n "$got" ]; then
    ok "received $(basename "$got") ($(hsize "$got"))  (medium: $med)"
    record RECV "$transport" "$label" PASS "$med" "$(basename "$got")"
  elif [ "$verdict" = NOFILE ]; then
    bad "$note  (medium seen: $med)"
    record RECV "$transport" "$label" FAIL "$med" "$note (BLE receive/BWU gap)"
  else
    # user stopped early, or timed out — get the phone-side truth
    bad "nothing saved on the PC."
    ask "On the phone, did it show the send as DONE? [y/N]:"; read -r ph
    if [[ "$ph" =~ ^[Yy] ]]; then record RECV "$transport" "$label" FAIL "$med" "phone showed DONE but PC saved nothing"
    else                         record RECV "$transport" "$label" FAIL "$med" "phone did not complete / not started"; fi
  fi
}

# ---- transport block -------------------------------------------------------
transport_block() {
  local key="$1" name="$2" mediums="$3" phone_setup="$4"
  [ -n "$ONLY_TRANSPORT" ] && [ "$ONLY_TRANSPORT" != "$key" ] && return
  head1 "TRANSPORT: $name"
  say "$phone_setup"
  ask "Set the phone as above, then [Enter] to begin this block (s=skip whole block):"
  read -r a; [ "$a" = s ] && return

  if [ "$DO_SEND" = 1 ]; then
    say "${DIM}--- SEND cells (phone on RECEIVE screen) ---${RST}"
    for pl in "${PAYLOADS[@]}"; do run_send "$name" "$mediums" "$pl"; done
  fi
  if [ "$DO_RECV" = 1 ]; then
    say "${DIM}--- RECEIVE cells (phone on SEND) ---${RST}"
    for pl in "${PAYLOADS[@]}"; do run_receive "$name" "$pl"; done
  fi
}

# ---- final matrix ----------------------------------------------------------
print_matrix() {
  head1 "RESULTS"
  {
    echo "open-quickshare interop test — $(date)"
    echo "phone filter: '${TARGET_NAME:-<any>}'   logs: $WORK"
    echo
    printf '%-8s %-14s %-7s %-8s %-16s %s\n' DIR TRANSPORT PAYLOAD STATUS MEDIUM NOTE
    printf '%-8s %-14s %-7s %-8s %-16s %s\n' ------- ------------- ------- ------- --------------- ----
    while IFS='|' read -r d t p s m n; do
      printf '%-8s %-14s %-7s %-8s %-16s %s\n' "$d" "$t" "$p" "$s" "$m" "$n"
    done < "$RESULTS"
    echo
    local pass fail skip
    pass=$(grep -c '|PASS|' "$RESULTS"); fail=$(grep -c '|FAIL|' "$RESULTS")
    skip=$(grep -c '|SKIP|' "$RESULTS")
    echo "PASS=$pass  FAIL=$fail  SKIP=$skip  (EXPECT-REFUSE counted separately)"
  } | tee "$REPORT"
  echo
  ok "report saved: $REPORT"
}

cleanup() { kill $(jobs -p) 2>/dev/null; }
trap cleanup EXIT

# ---- run -------------------------------------------------------------------
head1 "open-quickshare interop test"
say "This stops the running app so the test owns the Bluetooth adapter, then walks the matrix."
say "Bluetooth ON. Phone unlocked. You'll be prompted before every phone action."
ask "[Enter] to start:"; read -r _

stop_app; ok "app stopped (adapter free)"
gen_payloads

# same-LAN first (easiest), then Wi-Fi Direct, then pure BLE
transport_block lan    "same-LAN"     "5,8,3,10" \
  "Phone ON the SAME Wi-Fi network as this PC (large files should upgrade to Wi-Fi LAN; small ones stay on BLE by design)."
transport_block direct "Wi-Fi Direct" "5,8,3,10" \
  "Phone Wi-Fi RADIO ON but NOT connected to any network (forget/disable the network; large files use Wi-Fi Direct hosted by the phone)."
transport_block ble    "pure-BLE"     "10"   \
  "Phone Wi-Fi can be anything — we advertise BLE only, so everything stays on Bluetooth (the >1MB mp4 is expected to be refused up front)."

print_matrix

head1 "Restarting the app"
start_app; sleep 3
[ -n "$(app_pids)" ] && ok "app relaunched" || say "${DIM}app not detected — launch it yourself if needed${RST}"
say "Done."
