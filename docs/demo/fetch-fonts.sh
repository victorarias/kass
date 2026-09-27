#!/bin/sh
# Fonts are OFL-licensed and fetched rather than committed.
set -e
cd "$(dirname "$0")" && mkdir -p assets
curl -sfL -o assets/JetBrainsMono.ttf "https://github.com/google/fonts/raw/main/ofl/jetbrainsmono/JetBrainsMono%5Bwght%5D.ttf"
curl -sfL -o assets/Inter.ttf "https://github.com/google/fonts/raw/main/ofl/inter/Inter%5Bopsz,wght%5D.ttf"
