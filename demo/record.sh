#!/bin/sh
# Record docs/demo/oxutrm.webp from demo/oxutrm.toml. Run through `make demo`,
# which builds the release binary and passes its directory as $1. Further
# arguments go to `ansidrama record`, for example `--dump-png frames/`.
#
# The recording runs in a fresh scratch directory: its own home, config,
# client log and session registry. So it starts with no sessions and with the
# splash on, whatever the machine has, and the setting it changes does not
# touch the real config.
set -eu
bindir=$1
shift
demo=$(cd "$(dirname "$0")" && pwd)
scratch=$(mktemp -d "${TMPDIR:-/tmp}/oxutrm-demo.XXXXXX")

cleanup() {
    # The demo leaves its session running on purpose; end it here.
    OXUTRM_STATE_DIR="$scratch/run" "$bindir/oxutrm" host --list 2>/dev/null |
        while read -r _id _ pid _; do
            kill "$pid" 2>/dev/null || true
        done
    rm -rf "$scratch"
}
trap cleanup EXIT

mkdir -p "$scratch/home" "$scratch/config" "$scratch/state" "$scratch/run"
# The session's shell is a login shell in $HOME.
echo "PS1='host\$ '" > "$scratch/home/.bash_profile"

cd "$demo"
env HOME="$scratch/home" \
    XDG_CONFIG_HOME="$scratch/config" \
    XDG_STATE_HOME="$scratch/state" \
    OXUTRM_STATE_DIR="$scratch/run" \
    SHELL=/bin/bash \
    PATH="$demo/bin:$bindir:$PATH" \
    "${ANSIDRAMA:-ansidrama}" record oxutrm.toml "$@"
