#!/bin/sh
# Downloads the Inter typeface used by the UI into ui/fonts (needed before the first local build).
set -e
mkdir -p ui/fonts
curl -L --fail -o /tmp/inter.zip https://github.com/rsms/inter/releases/download/v4.0/Inter-4.0.zip
unzip -q -o /tmp/inter.zip -d /tmp/inter
for w in Regular Medium SemiBold Bold ExtraBold; do
  f=$(find /tmp/inter -name "Inter-$w.ttf" | head -1)
  [ -n "$f" ] || { echo "Inter-$w.ttf not found"; exit 1; }
  cp "$f" ui/fonts/
done
echo "fonts ready in ui/fonts"
