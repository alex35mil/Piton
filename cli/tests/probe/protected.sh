#!/bin/sh
# Host-only check of the sandbox a launch gets (docs/design.md, "Agent access"): resolves a scratch checkout's policy
# the way the daemon does (`piton sandbox show`), runs a shell through `piton sandbox-exec` with it, and reports what
# each attempt did. Run from a host shell (not a sandboxed session: sandboxes don't nest), after `mise run cli:build`:
# `sh cli/tests/probe/protected.sh`. Prints one line per attempt and exits 1 when any result differs from the
# expectation. It reads and connects to real paths of this user (credentials, the daemon's sockets). Outside its scratch
# folder it only tries writes that change nothing: it opens real files for appending without writing, and creates
# uniquely named entries that it removes afterwards, from the host, if the sandbox let them through.
set -eu

piton=$(cd "$(dirname "$0")/../.." && pwd)/target/debug/piton
scratch=$(mktemp -d "${TMPDIR:-/tmp}/piton-protected.XXXXXX")
trap 'chmod -R u+w "$scratch" 2>/dev/null; rm -rf "$scratch"' EXIT
# Replaced below, once the probe names exist, by one that also removes them.
# Seatbelt compares canonical paths (/private/var/..., not /var/...).
scratch=$(cd "$scratch" && pwd -P)
src="$scratch/repo"
outside="$scratch/outside"
mkdir -p "$src/.piton/.local" "$src/apps/sub" "$src/src" "$src/.agents/skills/s" "$outside"
git -C "$src" init -q
marker='# piton protected-file probe'
echo "$marker" > "$src/.piton/config.toml"
echo 'skill' > "$src/.agents/skills/s/SKILL.md"
echo 'a' > "$src/src/a.rs"

# The daemon's policy for this checkout, with the user's own global configuration (or PITON_CONFIG_HOME's).
if ! shown=$("$piton" sandbox show --workspace "$src"); then
    echo "piton sandbox show failed: fix the configuration it names, then run this again" >&2
    exit 1
fi
policy=$(printf '%s' "$shown" | python3 -I -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["policy"]))')
agent_dir=${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}
state=$HOME/.local/state/piton
config_home=${PITON_CONFIG_HOME:-$HOME/.config/piton}
# Names for the entries the denied attempts would create outside the scratch folder; removed below if they exist.
probe="piton-probe-$$"
cleanup_real() {
    rm -f "$HOME/$probe"
    rmdir "$HOME/Library/Caches/$probe" "$agent_dir/extensions/$probe" 2>/dev/null || true
}
trap 'cleanup_real; chmod -R u+w "$scratch" 2>/dev/null; rm -rf "$scratch"' EXIT

# Each attempt: expectation (ok | denied), a description, and a shell command run in the sandbox from the checkout.
cat > "$scratch/attempts.sh" <<'EOF'
attempt() {
    expect=$1; what=$2; shift 2
    # No terminal: a tool that can't write asks before giving up (`rm`: "override rw-r--r--?") and would wait.
    if sh -c "$*" </dev/null >/dev/null 2>"$ERR"; then got=ok; else got=denied; fi
    if [ "$got" = "$expect" ]; then mark=pass; else mark=FAIL; fi
    printf '%-5s %-7s %s\n' "$mark" "$got" "$what"
    # Why a check failed: the command's last error line.
    if [ "$mark" = FAIL ] && [ -s "$ERR" ]; then printf '      %s\n' "$(tail -n 1 "$ERR")"; fi
}
connect() { python3 -I -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])' "$1"; }
# Protected files.
attempt ok     "read a protected file"                  'cat .piton/config.toml'
attempt denied "overwrite a protected file"             'echo x > .piton/config.toml'
attempt denied "create a file in .piton/"               'echo y > .piton/new.json'
attempt denied "delete a protected file"                'rm -f .piton/config.toml'
attempt denied "rename .piton/"                         'mv -f .piton moved'
attempt denied "rename a protected file"                'mv -f .piton/config.toml .piton/p.toml'
attempt denied "hard-link a protected file out"         "ln .piton/config.toml $OUTSIDE/l && echo z >> $OUTSIDE/l"
attempt denied "symlink-replace a protected file"       'ln -sf /tmp/x .piton/config.toml'
attempt denied "create .pi/ at the root"                'mkdir -p .pi/extensions'
attempt denied "create .pi/ in a subfolder"             'mkdir -p apps/sub/.pi/extensions'
attempt denied "write .PI/ (another case)"              'mkdir -p .PI && echo x > .PI/settings.json'
attempt denied "write .PITON/ (another case)"           'echo x > .PITON/config.toml'
attempt ok     "write workflow artifacts"               'echo z > .piton/.local/x'
attempt ok     "write .piton/.LOCAL (another case)"     'echo z > .piton/.LOCAL/y'
attempt ok     "write a source file"                    'echo w > src/a.rs'
attempt ok     "write rules (not protected)"            'mkdir -p .agents/rules && echo r > .agents/rules/r.md'
attempt ok     "edit a skill (not protected)"           'echo more >> .agents/skills/s/SKILL.md'
attempt ok     "create a skill in a subfolder"          'mkdir -p apps/.agents/skills/x && echo s > apps/.agents/skills/x/SKILL.md'
attempt ok     "create a folder named like .pi but not" 'mkdir -p apps/.pix && echo q > apps/.pix/f'
# Moving a parent of a not-yet-existing protected folder is an ordinary write; the daemon guards its own writes
# against it (it never follows a symlink on an approved path).
attempt ok     "swap a parent folder for a symlink"     "mv apps apps.bak && ln -s $OUTSIDE apps"
# Reads: everything but credentials.
attempt ok     "read system files"                      'cat /etc/hosts'
attempt ok     "read git config"                        "test ! -e $HOME/.gitconfig || cat $HOME/.gitconfig"
attempt denied "read an SSH key"                        "cat $HOME/.ssh/id_*"
attempt denied "list the keychains' contents"           "cat $HOME/Library/Keychains/login.keychain-db"
attempt denied "read shell history"                     "cat $HOME/.zsh_history"
# Writes outside the workspace.
attempt ok     "write temp"                             'mktemp'
# `: >> file` opens a file for writing and writes nothing: denied or not, the file stays as it is.
attempt denied "write the home folder"                  "touch $HOME/$PROBE"
attempt denied "write a user cache"                     "mkdir $HOME/Library/Caches/$PROBE"
attempt denied "write pi's extensions"                  "mkdir $AGENT_DIR/extensions/$PROBE"
attempt denied "write pi's settings"                    ": >> $AGENT_DIR/settings.json"
attempt ok     "write pi's sessions"                    "mkdir -p $AGENT_DIR/sessions/$PROBE && rmdir $AGENT_DIR/sessions/$PROBE"
attempt denied "write the global configuration"         ": >> $CONFIG_HOME/config.toml"
# The daemon. The socket checks need python3 to run; if it can't, they all read "denied" and prove nothing.
attempt ok     "run python3 (for the socket checks)"   'python3 -I -c pass'
attempt denied "read the daemon's database"             "cat $STATE/piton.db"
attempt ok     "read Piton's logs"                      "ls $STATE/logs"
attempt denied "connect to the control socket"          "connect $STATE/piton.sock"
attempt denied "connect to it through a symlink"        "ln -s $STATE/piton.sock ctl.sock && connect ctl.sock"
attempt ok     "connect to the agent socket"            "connect $STATE/agent.sock"
EOF

cd "$src"
status=0
OUTSIDE="$outside" AGENT_DIR="$agent_dir" STATE="$state" CONFIG_HOME="$config_home" PROBE="$probe" ERR="$scratch/err" \
    PITON_SANDBOX_POLICY="$policy" "$piton" sandbox-exec -- sh "$scratch/attempts.sh" </dev/null \
    >"$scratch/out" 2>&1 || status=$?
cat "$scratch/out"
if [ "$status" -ne 0 ]; then
    echo "FAIL  the sandbox didn't start (piton sandbox-exec exited $status); nothing was tested" >&2
    exit 1
fi
# The integrity of the protected file itself, checked from outside the sandbox.
if [ "$(cat "$src/.piton/config.toml")" = "$marker" ]; then echo "pass  ok      protected file unchanged"; else echo "FAIL  changed protected file changed"; fi
! rg -q '^FAIL' "$scratch/out"
