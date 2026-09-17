#!/bin/sh
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "$#" -eq 0 ]; then
    exec "$here/w-vmm" run --disk "$here/data.qcow2"
fi
# Explicit arguments are passed to `w-vmm run`; relative disk paths use the caller's cwd.
exec "$here/w-vmm" run "$@"
