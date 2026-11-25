#!/bin/sh
# Fake `fnox` for tests (set as `[integrations.fnox] command`), mimicking fnox 1.48's output. What it does is decided by files
# in the folder it runs in (the workspace):
# - `fake-fnox-no-proxy`: no `[proxy]` applies (`proxy rules` fails with fnox's message);
# - `fake-fnox-broken`: the config is invalid (`proxy rules` and `proxy run` fail);
# - `fake-fnox-missing`: one secret name per line that fnox can't read: `proxy run` warns about each, as fnox does,
#   and starts the command anyway.
# `proxy run` sets FAKE_FNOX=1 and a placeholder in GITHUB_TOKEN, records its arguments in `fake-fnox-args`, and
# whether it saw PITON_WORKSPACE_MARKER in `fake-fnox-env`.
while [ "$#" -gt 0 ]; do
    case "$1" in
    --non-interactive) shift ;;
    *) break ;;
    esac
done
case "$1 $2" in
"proxy rules")
    if [ -e fake-fnox-no-proxy ]; then
        echo 'Error: fnox::config::error' >&2
        echo '  × Configuration error: No [proxy] configuration found; add one or more' >&2
        exit 1
    fi
    if [ -e fake-fnox-broken ]; then
        echo 'Error: fnox::config::invalid_toml' >&2
        echo '  × unknown field `hosts`' >&2
        exit 1
    fi
    echo 'egress: Permissive'
    ;;
"config-files "*)
    echo "$PWD/fnox.toml"
    ;;
"proxy run")
    shift 2
    printf '%s\n' "$@" > fake-fnox-args
    # What fnox itself sees: the daemon's base environment, never the workspace's.
    printf '%s\n' "FNOX_SEES_MARKER=${PITON_WORKSPACE_MARKER-unset}" > fake-fnox-env
    [ "$1" = --no-sandbox ] && shift
    [ "$1" = -- ] && shift
    if [ -e fake-fnox-missing ]; then
        while read -r name; do
            printf '\033[2m2026-11-18T18:45:38.441779Z\033[0m \033[33m WARN\033[0m fnox_core::secret_resolver: Error resolving secret '\''%s'\'': op: not signed in\n' "$name" >&2
        done < fake-fnox-missing
        while read -r name; do
            printf '2026-11-18T18:45:38.442312Z  WARN fnox::commands::proxy: fnox proxy: '\''%s'\'' is not set; requests that need it are refused until it is (for example, run `fnox set %s`)\n' "$name" "$name" >&2
        done < fake-fnox-missing
    fi
    FAKE_FNOX=1 GITHUB_TOKEN=fnox_proxy_0000 exec "$@"
    ;;
*)
    echo "fake fnox: unsupported command $*" >&2
    exit 1
    ;;
esac
