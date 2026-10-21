#!/bin/sh
# dpkg passes "configure" and the previously configured version, empty on a first install; rpm passes 1 on a first
# install. Upgrades keep an administrator's choice to disable the socket.
set -e

if { [ "$1" = configure ] && [ -z "$2" ]; } || [ "$1" = 1 ]; then
    systemctl --global enable contrd.socket >/dev/null 2>&1 || true
fi
