#!/bin/bash
# Rebuild the five trunk apps after narrowing the wasm-opt feature set, then repack + verify.
set -u
: > /tmp/rebuild.log
for app in wordcraft gridcraft deckcraft pdfcraft vectorcraft; do
  echo "=== $app build $(date +%T)" >> /tmp/rebuild.log
  ( cd /e/projects/tranxon/${app}-app/apps/${app}-web && trunk build --release --dist /e/projects/tranxon/${app}-app/dist/web > /tmp/trunk-${app}.log 2>&1 )
  echo "=== $app build exit=$? $(date +%T) wasm=$(ls -l /e/projects/tranxon/${app}-app/dist/web/*.wasm 2>/dev/null | awk '{print $5}')" >> /tmp/rebuild.log
done
echo BUILD-DONE >> /tmp/rebuild.log
bash /e/projects/tranxon/ACoworkAppGallery/build/package-all.sh
echo PACK-DONE >> /tmp/rebuild.log