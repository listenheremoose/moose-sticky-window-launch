#!/usr/bin/env bash
# Build, install to ~/.cargo/bin, and enable the systemd user service.
set -euo pipefail
cd "$(dirname "$0")"
cargo install --locked --path .
mkdir -p ~/.config/systemd/user
cp contrib/moose-sticky-window-launch.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now moose-sticky-window-launch.service
systemctl --user restart moose-sticky-window-launch.service
echo "Installed. Logs: journalctl --user -u moose-sticky-window-launch -f"
