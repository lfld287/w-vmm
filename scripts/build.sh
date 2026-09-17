#!/bin/sh
# Compatibility entry point.
set -eu
exec "$(dirname "$0")/../build.sh" "$@"
