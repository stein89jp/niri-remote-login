#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
mkdir -p generated
wayland-scanner client-header xdg-shell.xml generated/xdg-shell-client-protocol.h
wayland-scanner private-code xdg-shell.xml generated/xdg-shell-protocol.c
wayland-scanner client-header text-input-unstable-v3.xml generated/text-input-client-protocol.h
wayland-scanner private-code text-input-unstable-v3.xml generated/text-input-protocol.c
cc -Wall -Wextra -Werror -O2 -I generated dynamic-app.c generated/xdg-shell-protocol.c generated/text-input-protocol.c $(pkg-config --cflags --libs wayland-client xkbcommon) -o dynamic-app
