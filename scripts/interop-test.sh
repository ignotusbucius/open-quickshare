#!/usr/bin/env bash
# =============================================================================
# interop-test.sh — guided Quick Share interop test matrix for open-quickshare
#
#   Payloads : short text · long text · image · big mp4
#   Direction: SEND (PC -> phone)  and  RECEIVE (phone -> PC)
#   Transport: same-LAN Wi-Fi · Wi-Fi Direct (phone hosts) · pure BLE
#
# SEND cells drive the APP'S OWN engine headlessly (app_send: the same RQS
# service, discovery, dial ladder and medium logic the app runs); RECEIVE cells
# run the same engine via rx_service. Crucially the engine runs ONCE PER BLOCK
# (like the long-lived app), not once per file — starting/stopping the BLE
# stack per cell churns BlueZ until the link slows and the phone's handshake
# timer kills transfers at paired-key.
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
RECV_TIMEOUT="${RECV_TIMEOUT:-60}"            # idle seconds before a receive cell gives up

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

# ---- locate / verify engine binaries ---------------------------------------
need_build=0
for b in app_send rx_service; do [ -x "$EX_DIR/$b" ] || need_build=1; done
if [ "$need_build" = 1 ]; then
  head1 "Building test binaries (app_send, rx_service)"
  distrobox enter --name "$CONTAINER" -- bash -lc \
    "cd '$REPO/core_lib' && cargo build --release --features experimental --example app_send --example rx_service" \
    || { bad "example build failed"; exit 1; }
fi
for b in app_send rx_service; do
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

  # big mp4: > 1 MB on purpose so the engine advertises the Wi-Fi mediums for it.
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
    mp4   "$(hsize "$mp")" "> 1 MB -> engine offers Wi-Fi upgrade mediums"
}

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

teardown_hotspots() {
  local c
  for c in $(nmcli -t -f NAME connection show --active 2>/dev/null | grep -i '^DIRECT-'); do
    nmcli connection down "$c" >/dev/null 2>&1
  done
}

reset_adapter() {
  # Clear wedged BlueZ discovery state ("Operation already in progress" /
  # D-Bus timeouts) left over from earlier churn. Passive listening still
  # works when wedged, but StartDiscovery never succeeds again until a
  # power cycle — so every send would report NoReceiver.
  say "${DIM}power-cycling the Bluetooth adapter for a clean slate…${RST}"
  timeout 8 bluetoothctl power off >/dev/null 2>&1
  sleep 1
  timeout 8 bluetoothctl power on >/dev/null 2>&1
  sleep 2
}

adapter_wedged() {
  grep -qE 'Operation already in progress|org.freedesktop.DBus.Error.Timeout' "$1" 2>/dev/null
}

# ---- transport detection from a log (or log slice) --------------------------
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

# Slice a block-level send log to the Nth attempted send and detect its medium.
medium_for_attempt() {
  local log="$1" n="$2" tmp="$WORK/.sect"
  awk -v n="$n" '/connect_receiver: got SendInfo/{c++} c==n' "$log" > "$tmp"
  detect_medium "$tmp"
}

record() { printf '%s|%s|%s|%s|%s|%s\n' "$1" "$2" "$3" "$4" "$5" "$6" >> "$RESULTS"; }

# ---- SEND block: one engine instance for all payloads ----------------------
run_send_block() {
  local transport="$1"
  local out="$WORK/send-$transport.out" log="$WORK/send-$transport.log"
  head1 "SEND block · $transport"
  say "Phone: keep it on the Quick Share ${YEL}receive screen (Everyone)${RST} for all four files."
  say "You'll be prompted before each file — press ${YEL}Enter${RST} to send it, ${YEL}s+Enter${RST} to skip."
  echo

  local flist=() pl
  for pl in "${PAYLOADS[@]}"; do flist+=("$(payload_path "$pl")"); done

  STEP=1 RUST_LOG="info,rqs_lib=debug,mdns_sd=error" \
    "$EX_DIR/app_send" "${flist[@]}" 2>"$log" | tee "$out"
  echo

  if adapter_wedged "$log"; then
    bad "BlueZ discovery wedged during this block — resetting the adapter before continuing"
    reset_adapter
  fi

  # Parse per-file results; slice the shared log per attempted send for medium.
  local attempt=0
  for pl in "${PAYLOADS[@]}"; do
    local f; f="$(payload_path "$pl")"
    local res; res="$(grep -a -F "RESULT|$f|" "$out" | tail -1 | cut -d'|' -f3)"
    case "$res" in
      Skipped)    record SEND "$transport" "$pl" SKIP - "skipped"; continue ;;
      NoReceiver) record SEND "$transport" "$pl" FAIL - "receiver never discovered"; continue ;;
      "")         record SEND "$transport" "$pl" FAIL - "no result (engine aborted?)"; continue ;;
    esac
    attempt=$((attempt+1))
    local med; med="$(medium_for_attempt "$log" "$attempt")"
    if [ "$med" = "REFUSED-too-big" ]; then
      ok "$pl: refused up front as too large for BLE (expected without Wi-Fi)"
      record SEND "$transport" "$pl" "EXPECT-REFUSE" "$med" "correctly refused >1MB on BLE"
    elif [ "$res" = "Finished" ]; then
      ask "$pl: PC reports Finished (medium: $med) — did it actually arrive on the phone? [y/N]:"; read -r got
      if [[ "$got" =~ ^[Yy] ]]; then record SEND "$transport" "$pl" PASS "$med" "confirmed on phone"
      else                          record SEND "$transport" "$pl" FAIL "$med" "PC ok but phone did NOT receive"; fi
    else
      bad "$pl: $res  (medium: $med)"
      record SEND "$transport" "$pl" FAIL "$med" "final state $res"
    fi
  done
}

# ---- RECEIVE block: one receiver instance for all payloads ------------------
run_receive_block() {
  local transport="$1"
  local log="$WORK/recv-$transport.log"
  head1 "RECEIVE block · $transport"
  rm -f "$DL_DIR"/* 2>/dev/null
  RQS_DOWNLOAD_DIR="$DL_DIR" RQS_DEVICE_NAME="OQS Interop RX" \
    RUST_LOG="info,rqs_lib=debug,mdns_sd=error" \
    "$EX_DIR/rx_service" >"$log" 2>&1 &
  local rxpid=$!
  sleep 3
  say "Receiver is up as ${YEL}'OQS Interop RX'${RST} and stays up for the whole block."

  local pl
  for pl in "${PAYLOADS[@]}"; do
    echo
    say "— RECEIVE · $transport · ${YEL}$pl${RST} —"
    say "Phone: Quick Share → ${YEL}Send${RST} → pick the $pl → choose 'OQS Interop RX'."
    ask "[Enter] once you've STARTED the send on the phone   (s=skip):"; read -r a; echo
    if [ "$a" = s ]; then record RECV "$transport" "$pl" SKIP - "skipped"; continue; fi

    local off; off="$(stat -c%s "$log" 2>/dev/null || echo 0)"
    local before; before="$(find "$DL_DIR" -type f 2>/dev/null | sort)"

    say "  Waiting for the transfer — press ${YEL}Enter${RST} anytime to stop waiting early."
    # PASS requires the receiver to log completion (in THIS cell's log slice)
    # AND bytes on disk to stop growing; an existing file is an in-progress
    # transfer, never a verdict. Timeout counts only idle seconds.
    local waited=0 idle=0 verdict="" note="" last_sz=-1 fin_at=-1 got=""
    while :; do
      local sz; sz=$(du -sb "$DL_DIR" 2>/dev/null | cut -f1); sz=${sz:-0}
      if [ "$sz" != "$last_sz" ]; then last_sz="$sz"; idle=0; else idle=$((idle+2)); fi
      if tail -c "+$((off+1))" "$log" 2>/dev/null | grep -qE 'Transfer finished|TEXT RECEIVED'; then
        [ "$fin_at" -lt 0 ] && fin_at="$waited"
        if [ "$idle" -ge 4 ] || [ $((waited - fin_at)) -ge 10 ]; then
          got="$(comm -13 <(printf '%s\n' "$before") <(find "$DL_DIR" -type f 2>/dev/null | sort) | head -1)"
          if [ -n "$got" ] && [ -s "$got" ]; then verdict=PASS
          else verdict=NOFILE; note="receiver reported finished but wrote NO new file"; fi
          break
        fi
      fi
      [ "$idle" -ge "$RECV_TIMEOUT" ] && { verdict=TIMEOUT; break; }
      if read -t 2 -N 1 -r _k 2>/dev/null; then verdict=STOP; break; fi
      waited=$((waited+2)); printf '\r  …%ss (%s bytes on disk)   ' "$waited" "$sz"
    done
    echo
    local med; med="$(tail -c "+$((off+1))" "$log" 2>/dev/null > "$WORK/.rslice"; detect_medium "$WORK/.rslice")"

    if [ "$verdict" = PASS ]; then
      ok "received $(basename "$got") ($(hsize "$got"))  (medium: $med)"
      record RECV "$transport" "$pl" PASS "$med" "$(basename "$got")"
    elif [ "$verdict" = NOFILE ]; then
      bad "$note  (medium seen: $med)"
      record RECV "$transport" "$pl" FAIL "$med" "$note"
    else
      bad "nothing completed on the PC."
      ask "On the phone, did it show the send as DONE? [y/N]:"; read -r ph
      if [[ "$ph" =~ ^[Yy] ]]; then record RECV "$transport" "$pl" FAIL "$med" "phone showed DONE but PC saved nothing"
      else                         record RECV "$transport" "$pl" FAIL "$med" "phone did not complete / not started"; fi
    fi
  done

  kill -INT "$rxpid" 2>/dev/null; wait "$rxpid" 2>/dev/null
  teardown_hotspots
}

# ---- transport block --------------------------------------------------------
transport_block() {
  local key="$1" name="$2" phone_setup="$3"
  [ -n "$ONLY_TRANSPORT" ] && [ "$ONLY_TRANSPORT" != "$key" ] && return
  head1 "TRANSPORT: $name"
  say "$phone_setup"
  ask "Set the phone as above, then [Enter] to begin this block (s=skip whole block):"
  read -r a; [ "$a" = s ] && return

  [ "$DO_SEND" = 1 ] && run_send_block "$name"
  [ "$DO_RECV" = 1 ] && run_receive_block "$name"
}

# ---- final matrix ----------------------------------------------------------
print_matrix() {
  head1 "RESULTS"
  {
    echo "open-quickshare interop test — $(date)"
    echo "phone filter: '${TARGET_NAME:-<any>}'   logs: $WORK"
    echo
    printf '%-8s %-14s %-7s %-14s %-22s %s\n' DIR TRANSPORT PAYLOAD STATUS MEDIUM NOTE
    printf '%-8s %-14s %-7s %-14s %-22s %s\n' ------- ------------- ------- ------------- --------------------- ----
    while IFS='|' read -r d t p s m n; do
      printf '%-8s %-14s %-7s %-14s %-22s %s\n' "$d" "$t" "$p" "$s" "$m" "$n"
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

# Ctrl+C (or TERM) at ANY point finalizes instead of discarding: it sweeps
# results the engine printed but the script hadn't parsed yet, prints and
# saves the matrix of everything done so far, and relaunches the app.
FINALIZED=0
finalize() {
  [ "$FINALIZED" = 1 ] && exit 130
  FINALIZED=1
  trap - INT TERM
  kill $(jobs -p) 2>/dev/null
  local o t f res pl _tag
  for o in "$WORK"/send-*.out; do
    [ -f "$o" ] || continue
    t="$(basename "$o")"; t="${t#send-}"; t="${t%.out}"
    while IFS='|' read -r _tag f res; do
      pl="$(basename "$f")"; pl="${pl%%.*}"; [ "$pl" = big ] && pl=mp4
      grep -q "^SEND|$t|$pl|" "$RESULTS" 2>/dev/null && continue
      case "$res" in
        Finished) record SEND "$t" "$pl" PASS "?" "finished (phone-side unconfirmed; run interrupted)";;
        Skipped)  record SEND "$t" "$pl" SKIP - "skipped";;
        *)        record SEND "$t" "$pl" FAIL "?" "final state $res (run interrupted)";;
      esac
    done < <(grep -a '^RESULT|' "$o")
  done
  teardown_hotspots
  if [ -s "$RESULTS" ]; then
    echo; say "${YEL}Interrupted — saving the results gathered so far.${RST}"
    print_matrix
  else
    say "interrupted before any cell finished — nothing to report"
  fi
  start_app
  exit 130
}
trap finalize INT TERM
cleanup() { [ "$FINALIZED" = 1 ] || kill $(jobs -p) 2>/dev/null; }
trap cleanup EXIT

# ---- run -------------------------------------------------------------------
head1 "open-quickshare interop test"
say "This stops the running app so the test owns the Bluetooth adapter, then walks the matrix."
say "Bluetooth ON. Phone unlocked. You'll be prompted before every phone action."
ask "[Enter] to start:"; read -r _

stop_app; ok "app stopped (adapter free)"
reset_adapter
rm -f "$WORK"/send-*.out   # stale engine outputs would pollute an interrupt sweep
gen_payloads

# same-LAN first (easiest), then Wi-Fi Direct, then pure BLE
transport_block lan    "same-LAN" \
  "Phone ON the SAME Wi-Fi network as this PC (large files should upgrade to Wi-Fi; small ones stay on BLE by design)."
transport_block direct "Wi-Fi Direct" \
  "Phone Wi-Fi RADIO ON but NOT connected to any network (forget/disable the network; large transfers use a Wi-Fi Direct group)."
transport_block ble    "pure-BLE" \
  "Phone Wi-Fi fully OFF (radio off). Everything stays on Bluetooth; large receives may still see the phone force a hotspot."

print_matrix

head1 "Restarting the app"
teardown_hotspots
start_app; sleep 3
[ -n "$(app_pids)" ] && ok "app relaunched" || say "${DIM}app not detected — launch it yourself if needed${RST}"
say "Done."
