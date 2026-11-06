#!/usr/bin/env bash
set -euo pipefail
systemctl --user disable --now moose-sticky-window-launch.service || true
rm -f ~/.config/systemd/user/moose-sticky-window-launch.service
systemctl --user daemon-reload
cargo uninstall moose-sticky-window-launch || true
echo "Removed. Remembered placements kept in ~/.local/state/moose-sticky-window-launch/"
