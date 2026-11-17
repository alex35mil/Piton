#!/bin/sh
# Host-only check of the sandbox a launch gets (docs/design.md, "Agent access"): resolves a scratch checkout's policy
# the way the daemon does (`piton sandbox show`), runs a shell through `piton sandbox-exec` with it, and reports what
# each attempt did. Run from a host shell (not a sandboxed session: sandboxes don't nest), after `mise run cli:build`:
# `sh cli/tests/probe/protected.sh`. Prints one line per attempt and exits 1 when any result differs from the
# expectation. It reads and connects to real paths of this user (credentials, the daemon's sockets) but writes only in
# its scratch folder.
set -eu

piton=$(cd "$(dirname "$0")/../.." && pwd)/target/debug/piton
scratch=$(mktemp -d "${TMPDIR:-/tmp}/piton-protected.XXXXXX")
trap 'chmod -R u+w "$scratch" 2>/dev/null; rm -rf "$scratch"' EXIT
# Seatbelt compares canonical paths (/private/var/..., not /var/...).
scratch=$(cd "$scratch" && pwd -P)
src="$scratch/repo"
outside="$scratch/outside"
mkdir -p "$src/.piton/.local" "$src/apps/sub" "$src/src" "$src/.agents/skills/s" "$outside"
git -C "$src" init -q
echo '{}' > "$src/.piton/config.toml"
echo 'skill' > "$src/.agents/skills/s/SKILL.md"
echo 'a' > "$src/src/a.rs"

# The daemon's policy for this checkout, with the user's own global configuration.
policy=$("$piton" sandbox show --workspace "$src" | python3 -I -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["policy"]))')
agent_dir=${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}
state=$HOME/.local/state/piton

# Each attempt: expectation (ok | denied), a description, and a shell command run in the sandbox from the checkout.
cat > "$scratch/attempts.sh" <<'EOF'
attempt() {
    expect=$1; what=$2; shift 2
    # No terminal: a tool that can't write asks before giving up (`rm`: "override rw-r--r--?") and would wait.
    if sh -c "$*" </dev/null >/dev/null 2>&1; then got=ok; else got=denied; fi
    if [ "$got" = "$expect" ]; then mark=pass; else mark=FAIL; fi
    printf '%-5s %-7s %s\n' "$mark" "$got" "$what"
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
attempt denied "write the home folder"                  "touch $HOME/piton-probe"
attempt denied "write a user cache"                     "mkdir -p $HOME/Library/Caches/piton-probe"
attempt denied "write pi's extensions"                  "mkdir -p $AGENT_DIR/extensions/sessions"
attempt denied "write pi's settings"                    "echo x >> $AGENT_DIR/settings.json"
attempt ok     "write pi's sessions"                    "mkdir -p $AGENT_DIR/sessions/piton-probe && rmdir $AGENT_DIR/sessions/piton-probe"
attempt denied "write the global configuration"         "echo x >> ${PITON_CONFIG_HOME:-$HOME/.config/piton}/config.toml"
# The daemon.
attempt denied "read the daemon's database"             "cat $STATE/piton.db"
attempt ok     "read Piton's logs"                      "ls $STATE/logs"
attempt denied "connect to the control socket"          "connect $STATE/piton.sock"
attempt denied "connect to it through a symlink"        "ln -s $STATE/piton.sock ctl.sock && connect ctl.sock"
attempt ok     "connect to the agent socket"            "connect $STATE/agent.sock"
EOF

cd "$src"
OUTSIDE="$outside" AGENT_DIR="$agent_dir" STATE="$state" PITON_SANDBOX_POLICY="$policy" \
    "$piton" sandbox-exec -- sh "$scratch/attempts.sh" </dev/null | tee "$scratch/out"
# The integrity of the protected file itself, checked from outside the sandbox.
if [ "$(cat "$src/.piton/config.toml")" = '{}' ]; then echo "pass  ok      protected file unchanged"; else echo "FAIL  changed protected file changed"; fi
! rg -q '^FAIL' "$scratch/out"
