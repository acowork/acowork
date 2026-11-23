#!/bin/bash
# Local acceptance for a packaged .app:
#  1. unpack + run the binary, check /health, /ui/, and the wasm's MIME
#  2. boot the page in headless Chrome (catches wasm/js errors the HTTP checks cannot see)
#  3. run it inside build/bridge-harness.html in the SAME sandbox ACowork uses, and check the
#     bridge's ready/openFile/saveFile plumbing
# Usage: bash build/verify-apps.sh [app ...]
set -u
PKG=/e/projects/tranxon/ACowork/build/packages
WORK=/e/projects/tranxon/ACowork/build/verify
HARNESS=/e/projects/tranxon/ACowork/build/bridge-harness.html
CHROME="/c/Program Files/Google/Chrome/Application/chrome.exe"
mkdir -p "$WORK"
: > /tmp/verify.log

apps=${*:-wordcraft gridcraft deckcraft pdfcraft vectorcraft lightcraft}
# harness page over http: file:// + a sandboxed cross-origin iframe does not always dump
( cd "$(dirname "$HARNESS")" && python -m http.server 18099 > /dev/null 2>&1 & )
sleep 1
for app in $apps; do
  pkg=$(ls "$PKG"/$app-*.app 2>/dev/null | head -1)
  [ -n "$pkg" ] || { echo "$app: NO PACKAGE" >> /tmp/verify.log; continue; }
  dir=$WORK/$app
  rm -rf "$dir"; mkdir -p "$dir"
  "C:/Windows/System32/tar.exe" -xf "$(cygpath -w "$pkg")" -C "$(cygpath -w "$dir")" || { echo "$app: UNPACK FAIL" >> /tmp/verify.log; continue; }

  log=/tmp/run-$app.log
  exe=$(basename "$(ls "$dir"/bin/*.exe | head -1)")
  ( cd "$dir" && "./bin/$exe" > "$log" 2>&1 & )
  port=""
  for _ in $(seq 1 40); do
    sleep 0.5
    port=$(grep -oE "listening on 127\.0\.0\.1:[0-9]+" "$log" | tail -1 | grep -oE "[0-9]+$")
    [ -n "$port" ] && break
  done
  [ -n "$port" ] || { echo "$app: NO LISTEN ($(tail -2 "$log" | tr '\n' ' '))" >> /tmp/verify.log; continue; }

  health=$(curl -s -m 5 -o /dev/null -w "%{http_code}" "http://127.0.0.1:$port/health")
  html=$(curl -s -m 10 "http://127.0.0.1:$port/ui/")
  ui=$([ -n "$html" ] && echo 200 || echo 000)
  wf=$(echo "$html" | grep -oE '[A-Za-z0-9_-]+\.wasm' | head -1)
  wasm=$(curl -s -m 90 -o /dev/null -w "%{http_code}/%{content_type}" "http://127.0.0.1:$port/ui/$wf")

  # sample bytes for the bridge test: a real PNG for the image app, else the same bytes (a format
  # the app may reject — a rejection proves the bridge delivered, which is what this checks).
  case $app in
    lightcraft) sample="$dir/assets/icon.png"; sname=sample.png ;;
    vectorcraft) printf '<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><rect width="64" height="64" fill="#0af"/></svg>' > /tmp/sample.svg; sample=/tmp/sample.svg; sname=sample.svg ;;
    wordcraft) sample="$dir/assets/icon.png"; sname=sample.docx ;;
    gridcraft) sample="$dir/assets/icon.png"; sname=sample.xlsx ;;
    deckcraft) sample="$dir/assets/icon.png"; sname=sample.pptx ;;
    pdfcraft) sample="$dir/assets/icon.png"; sname=sample.pdf ;;
  esac
  mkdir -p "$(dirname "$HARNESS")/samples"
  cp "$sample" "$(dirname "$HARNESS")/samples/$app-$(basename "$sname")"
  spat="samples/$app-$(basename "$sname")"

  timeout 120 "$CHROME" --headless=new --enable-unsafe-swiftshader --enable-logging=stderr --v=0 \
    --virtual-time-budget=25000 --dump-dom "http://127.0.0.1:$port/ui/" > /tmp/dom-$app.html 2> /tmp/chrome-$app.log
  err=$(grep -oE "Uncaught [A-Za-z]+Error[^\"]*" /tmp/chrome-$app.log | head -2 | tr '\n' ';')

  timeout 150 "$CHROME" --headless=new --enable-unsafe-swiftshader --enable-logging=stderr --v=0 \
    --allow-file-access-from-files --virtual-time-budget=30000 --dump-dom \
    "http://127.0.0.1:18099/bridge-harness.html?app=$app&src=http%3A%2F%2F127.0.0.1%3A$port%2Fui%2F&name=$sname&sample=$spat" \
    > /tmp/harness-$app.html 2> /tmp/harness-$app.log
  title=$(grep -oE "<title>bridge-harness:[^<]*" /tmp/harness-$app.html | head -1 | sed 's/<title>//')
  herr=$(grep -oE "Uncaught [A-Za-z]+Error[^\"]*" /tmp/harness-$app.log | head -1)

  echo "$app port=$port health=$health ui=$ui wasm=$wasm" >> /tmp/verify.log
  echo "    page: err=${err:-none}" >> /tmp/verify.log
  echo "    bridge: ${title:-NO TITLE} herr=${herr:-none}" >> /tmp/verify.log

  powershell -NoProfile -Command "Get-CimInstance Win32_Process -Filter \"Name='$exe'\" | Where-Object { \$_.CommandLine -like '*verify*' } | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force }" >/dev/null 2>&1
  sleep 1
done
echo VERIFY-DONE >> /tmp/verify.log