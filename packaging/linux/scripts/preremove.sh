#!/bin/sh
# dpkg passes "remove" and rpm passes 0 only when the package goes away, not on an upgrade.
set -e

if [ "$1" = remove ] || [ "$1" = 0 ]; then
    systemctl --global disable contrd.socket >/dev/null 2>&1 || true
fi
