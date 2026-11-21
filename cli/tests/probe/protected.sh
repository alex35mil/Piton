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
mkdir -p "$src/.piton/.local" "$src/apps/sub" "$src/apps/.pi" "$src/src" "$src/.agents/skills/s" "$outside"
git -C "$src" init -q
# A commit, so the sandboxed Git checks below have a branch and a HEAD; a nested repository for the any-depth rules.
git -C "$src" -c user.email=p@p -c user.name=p commit -q --allow-empty -m probe
git -C "$src/apps/sub" init -q
# A grant through a link the sandbox could have made (here, in the checkout) mustn't reach the link's target: a folder
# outside every write the sandbox has, created and removed by this script.
probe="piton-probe-$$"
linked="$HOME/$probe-linked"
# Names for the entries the denied attempts would create outside the scratch folder; removed below if they exist.
agent_dir=${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}
cleanup_real() {
    rm -rf "$linked"
    rm -f "$HOME/$probe"
    rmdir "$HOME/Library/Caches/$probe" "$agent_dir/extensions/$probe" 2>/dev/null || true
    rmdir "$agent_dir/TRUST.JSON.LOCK" 2>/dev/null || true
}
listener=
# The listener is waited for after it's killed, so the shell doesn't report its end ("Terminated: 15").
trap 'test -z "$listener" || { kill "$listener" 2>/dev/null; wait "$listener" 2>/dev/null; }; cleanup_real; chmod -R u+w "$scratch" 2>/dev/null; rm -rf "$scratch"' EXIT
mkdir "$linked"
ln -s "$linked" "$src/cache"
marker="# piton protected-file probe
[sandbox]
write = [\"$src/cache\"]"
printf '%s\n' "$marker" > "$src/.piton/config.toml"
# Unix sockets the host listens on, in the scratch folder (temp, which the sandbox writes): one no entry names, and one
# the checkout's own config.local.toml lists, which must stay in place.
mkdir -p "$scratch/host" "$scratch/listed"
host_sock="$scratch/host/listen.sock"
listed_sock="$scratch/listed/ok.sock"
printf '[sandbox]\nconnect = ["%s"]\n' "$listed_sock" > "$src/.piton/config.local.toml"
python3 -I -c '
import signal, socket, sys, threading
def serve(path):
    s = socket.socket(socket.AF_UNIX); s.bind(path); s.listen(16)
    while True: s.accept()[0].close()
for path in sys.argv[1:]: threading.Thread(target=serve, args=(path,), daemon=True).start()
signal.pause()
' "$host_sock" "$listed_sock" > /dev/null 2>&1 &
listener=$!
while [ ! -S "$host_sock" ] || [ ! -S "$listed_sock" ]; do sleep 0.1; done
echo 'skill' > "$src/.agents/skills/s/SKILL.md"
echo 'a' > "$src/src/a.rs"

# The daemon's policy for this checkout, with the user's own global configuration (or PITON_CONFIG_HOME's).
if ! shown=$("$piton" sandbox show --workspace "$src"); then
    echo "piton sandbox show failed: fix the configuration it names, then run this again" >&2
    exit 1
fi
policy=$(printf '%s' "$shown" | python3 -I -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["policy"]))')
state=$HOME/.local/state/piton
state_other_case=$HOME/.LOCAL/STATE/PITON
config_home=${PITON_CONFIG_HOME:-$HOME/.config/piton}

# Every check runs in a shell of its own, so helpers are files, not shell functions.
connect="$scratch/connect.py"
echo 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])' > "$connect"
# Checks of a real path: "skip" when it doesn't exist (a missing file would fail as if denied), otherwise the given
# expectation, or the policy's own decision for paths the user's configuration may lift with `read`.
needs() { if [ -e "$1" ] || [ -S "$1" ]; then echo "$2"; else echo skip; fi; }
policy_says() {
    if "$piton" sandbox why --workspace "$src" "$@" >/dev/null 2>&1; then echo ok; else echo denied; fi
}
bind="$scratch/bind.py"
echo 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])' > "$bind"
expect_bind=$(policy_says --bind "$src/probe.sock")
expect_ssh_agent=$(needs "${SSH_AUTH_SOCK:-/nonexistent}" "$(policy_says --connect "${SSH_AUTH_SOCK:-/nonexistent}")")
ssh_pub=$(ls "$HOME"/.ssh/*.pub "$HOME"/.ssh/*/*.pub 2>/dev/null | head -n 1 || true)
expect_ssh_pub=$(needs "${ssh_pub:-/nonexistent}" "$(policy_says "${ssh_pub:-/nonexistent}")")
# A private key anywhere under ~/.ssh (keys often live in a subfolder), read on the host: the first file, four levels
# deep at most, whose first line is a PEM or OpenSSH private-key header.
ssh_key=$(python3 -I -c '
import os, sys
root = os.path.expanduser("~/.ssh")
for dirpath, dirs, files in os.walk(root):
    if dirpath[len(root):].count(os.sep) >= 4:
        dirs[:] = []
    for name in sorted(files):
        path = os.path.join(dirpath, name)
        try:
            with open(path, "rb") as f:
                first = f.readline(200)
        except OSError:
            continue
        if first.startswith(b"-----BEGIN") and b"PRIVATE KEY" in first:
            print(path)
            sys.exit(0)
' 2>/dev/null || true)
keychain="$HOME/Library/Keychains/login.keychain-db"
expect_ssh_key=$(needs "${ssh_key:-/nonexistent}" denied)
expect_keychain=$(needs "$keychain" "$(policy_says "$keychain")")
expect_history=$(needs "$HOME/.zsh_history" denied)
expect_database=$(needs "$state/piton.db" denied)
expect_control=$(needs "$state/piton.sock" denied)
expect_agent=$(needs "$state/agent.sock" ok)
# pi's settings linked into a dotfiles repository: the real file is closed too (a session there proposes changes).
settings_target=$(test -L "$agent_dir/settings.json" && python3 -I -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$agent_dir/settings.json" || true)
expect_settings_target=$(needs "${settings_target:-/nonexistent}" denied)
# And the folder holding it can't be moved aside and replaced. The attempt moves it to a name beside it and back, so
# if the sandbox let it through nothing is lost; the host restores it below if the move back didn't happen.
settings_folder=$(test -n "$settings_target" && dirname "$settings_target" || true)
settings_moved="${settings_folder}.$probe"

# Each attempt: expectation (ok | denied), a description, and a shell command run in the sandbox from the checkout.
cat > "$scratch/attempts.sh" <<'EOF'
attempt() {
    expect=$1; what=$2; shift 2
    if [ "$expect" = skip ]; then printf '%-5s %-7s %s\n' skip - "$what (not on this Mac)"; return; fi
    # No terminal: a tool that can't write asks before giving up (`rm`: "override rw-r--r--?") and would wait.
    if sh -c "$*" </dev/null >/dev/null 2>"$ERR"; then got=ok; else got=denied; fi
    if [ "$got" = "$expect" ]; then mark=pass; else mark=FAIL; fi
    printf '%-5s %-7s %s\n' "$mark" "$got" "$what"
    # Why a check failed: the command's last error line.
    if [ "$mark" = FAIL ] && [ -s "$ERR" ]; then printf '      %s\n' "$(tail -n 1 "$ERR")"; fi
}
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
# pi locks .pi/settings.json to read it: the lock folder alone is writable.
attempt ok     "take and drop the project settings lock" 'mkdir apps/.pi/settings.json.lock && touch apps/.pi/settings.json.lock && rmdir apps/.pi/settings.json.lock'
attempt denied "write inside the settings lock"         'mkdir apps/.pi/settings.json.lock && echo x > apps/.pi/settings.json.lock/f'
# fnox's config files, at any depth and in any case: which secrets go where.
attempt denied "create fnox.toml"                       'echo x > fnox.toml'
attempt denied "create .config/fnox.local.toml"         'mkdir -p .config && echo x > .config/fnox.local.toml'
attempt denied "create a profile's fnox file deeper"    'echo x > apps/sub/.fnox.dev.toml'
attempt denied "write FNOX.TOML (another case)"         'echo x > FNOX.TOML'
attempt ok     "write a file named like fnox but not"   'echo x > fnox.toml.bak && echo y > myfnox.toml'
attempt ok     "write workflow artifacts"               'echo z > .piton/.local/x'
attempt ok     "write .piton/.LOCAL (another case)"     'echo z > .piton/.LOCAL/y'
attempt ok     "write a source file"                    'echo w > src/a.rs'
# Git's hooks and configuration: Git on the host runs what they name. The rest of .git stays writable.
attempt denied "add a Git hook"                         'printf "#!/bin/sh\n" > .git/hooks/post-merge'
attempt denied "write .git/config"                      'echo "[core]" >> .git/config'
attempt denied "set core.fsmonitor"                     'git config core.fsmonitor /tmp/x'
attempt denied "write .git/commondir"                   'echo /tmp > .git/commondir'
attempt denied "replace .git"                           'mv .git git.bak'
attempt denied "write .GIT/HOOKS (another case)"        'echo x > .GIT/HOOKS/pre-commit'
attempt denied "add a nested repository's hook"         'echo x > apps/sub/.git/hooks/post-checkout'
attempt ok     "commit"                                 'echo c > src/c.rs && git add src/c.rs && git -c user.email=p@p -c user.name=p commit -q -m c'
attempt ok     "create and switch branches"             'git switch -q -c probe-branch && git switch -q -'
attempt ok     "add and remove a worktree in .piton/.local" 'git worktree add -q --detach .piton/.local/pull-requests/1/rebase && git worktree remove --force .piton/.local/pull-requests/1/rebase'
attempt ok     "write rules (not protected)"            'mkdir -p .agents/rules && echo r > .agents/rules/r.md'
attempt ok     "edit a skill (not protected)"           'echo more >> .agents/skills/s/SKILL.md'
attempt ok     "create a skill in a subfolder"          'mkdir -p apps/.agents/skills/x && echo s > apps/.agents/skills/x/SKILL.md'
attempt ok     "create a folder named like .pi but not" 'mkdir -p apps/.pix && echo q > apps/.pix/f'
# Moving a parent of a not-yet-existing protected folder is an ordinary write; the daemon guards its own writes
# against it (it never follows a symlink on an approved path).
attempt ok     "swap a parent folder for a symlink"     "mv apps apps.bak && ln -s $OUTSIDE apps"
# A granted path that's a symlink grants the link, not what it points to.
attempt denied "write through a granted symlink"        'touch cache/x'
# Reads: everything but credentials.
attempt ok     "read system files"                      'cat /etc/hosts'
attempt ok     "read git config"                        "test ! -e $HOME/.gitconfig || cat $HOME/.gitconfig"
attempt "$EXPECT_SSH_KEY" "read an SSH key"              "cat '$SSH_KEY'"
# Denied by default; allowed when the configuration lists the keychains in `read`.
attempt "$EXPECT_KEYCHAIN" "read the login keychain"     "cat $KEYCHAIN"
attempt "$EXPECT_HISTORY" "read shell history"           "cat $HOME/.zsh_history"
# Other case: APFS is case-insensitive, and so is Seatbelt's matching there; denies and the allows inside them hold.
attempt "$EXPECT_SSH_KEY" "read an SSH key in another case" 'cat "$HOME/.SSH/${SSH_KEY#"$HOME/.ssh/"}"'
attempt "$EXPECT_HISTORY" "read shell history in another case" "cat $HOME/.ZSH_HISTORY"
# Writes outside the workspace.
attempt ok     "write temp"                             'mktemp'
# `: >> file` opens a file for writing and writes nothing: denied or not, the file stays as it is.
attempt denied "write the home folder"                  "touch $HOME/$PROBE"
attempt denied "write a user cache"                     "mkdir $HOME/Library/Caches/$PROBE"
attempt denied "write pi's extensions"                  "mkdir $AGENT_DIR/extensions/$PROBE"
attempt denied "write pi's settings"                    ": >> $AGENT_DIR/settings.json"
attempt "$EXPECT_SETTINGS_TARGET" "write pi's settings at their linked real path" ": >> '$SETTINGS_TARGET'"
attempt "$EXPECT_SETTINGS_TARGET" "rename the folder holding pi's linked settings" "mv '$SETTINGS_FOLDER' '$SETTINGS_MOVED' && mv '$SETTINGS_MOVED' '$SETTINGS_FOLDER'"
attempt ok     "write pi's sessions"                    "mkdir -p $AGENT_DIR/sessions/$PROBE && rmdir $AGENT_DIR/sessions/$PROBE"
attempt denied "write pi's project trust"               ": >> $AGENT_DIR/trust.json"
attempt denied "write pi's MCP servers"                 ": >> $AGENT_DIR/mcp.json"
# The locks pi takes to read its settings and trust, and an MCP server's token-refresh lock; never left behind.
attempt ok     "take and drop pi's settings lock"       "mkdir $AGENT_DIR/settings.json.lock && touch $AGENT_DIR/settings.json.lock && rmdir $AGENT_DIR/settings.json.lock"
attempt ok     "take and drop pi's trust lock"          "mkdir $AGENT_DIR/trust.json.lock && rmdir $AGENT_DIR/trust.json.lock"
attempt ok     "take and drop an MCP refresh lock"      "mkdir $AGENT_DIR/mcp-auth-refresh-$PROBE.lock && rmdir $AGENT_DIR/mcp-auth-refresh-$PROBE.lock"
attempt ok     "take and drop pi's trust lock in another case" "mkdir $AGENT_DIR/TRUST.JSON.LOCK && rmdir $AGENT_DIR/TRUST.JSON.LOCK"
attempt denied "write pi's extensions in another case"  "mkdir $AGENT_DIR/EXTENSIONS/$PROBE"
attempt denied "write the global configuration"         ": >> $CONFIG_HOME/config.toml"
# The daemon. The socket checks need python3 to run; if it can't, they all read "denied" and prove nothing.
attempt ok     "run python3 (for the socket checks)"   'python3 -I -c pass'
attempt "$EXPECT_DATABASE" "read the daemon's database"  "cat $STATE/piton.db"
attempt ok     "read Piton's logs"                      "ls $STATE/logs"
attempt "$EXPECT_DATABASE" "read the daemon's database in another case" "cat $STATE_OTHER_CASE/PITON.DB"
attempt ok     "read Piton's logs in another case"      "ls $STATE_OTHER_CASE/LOGS"
attempt "$EXPECT_CONTROL" "connect to the control socket in another case" "python3 -I $CONNECT $STATE_OTHER_CASE/PITON.SOCK"
attempt "$EXPECT_AGENT" "connect to the agent socket in another case" "python3 -I $CONNECT $STATE_OTHER_CASE/AGENT.SOCK"
attempt "$EXPECT_CONTROL" "connect to the control socket" "python3 -I $CONNECT $STATE/piton.sock"
attempt "$EXPECT_CONTROL" "connect to it through a symlink" "ln -s $STATE/piton.sock ctl.sock && python3 -I $CONNECT ctl.sock"
attempt "$EXPECT_AGENT" "connect to the agent socket"    "python3 -I $CONNECT $STATE/agent.sock"
# Unix sockets: closed unless an entry names them. TCP and DNS stay open (#228 owns the network).
attempt ok     "look up a host name (DNS)"             "python3 -I -c 'import socket; socket.getaddrinfo(\"github.com\", 443)'"
attempt ok     "connect over TCP"                      "python3 -I -c 'import socket; socket.create_connection((\"github.com\", 443), 10).close()'"
attempt ok     "listen on TCP localhost"               "python3 -I -c 'import socket; s=socket.socket(); s.bind((\"127.0.0.1\", 0)); s.listen()'"
attempt denied "connect to a host process's socket"    "python3 -I $CONNECT $HOST_SOCK"
attempt denied "connect to it through a hard link"     "ln $HOST_SOCK hl.sock && python3 -I $CONNECT hl.sock"
attempt ok     "connect to a socket an entry names"    "python3 -I $CONNECT $LISTED_SOCK"
attempt denied "move another socket onto a named one"  "mv $LISTED_SOCK $OUTSIDE/moved.sock || true; ln -f $HOST_SOCK $LISTED_SOCK"
attempt denied "rename a named socket's folder"        "mv $(dirname $LISTED_SOCK) $OUTSIDE/moved-folder"
attempt "$EXPECT_BIND" "create a socket in the workspace" "python3 -I $BIND probe.sock"
attempt "$EXPECT_SSH_AGENT" "list the SSH agent's keys" "ssh-add -l"
attempt "$EXPECT_SSH_PUB" "read an SSH public key"    "cat '$SSH_PUB'"
EOF

cd "$src"
status=0
EXPECT_SSH_KEY="$expect_ssh_key" SSH_KEY="$ssh_key" EXPECT_KEYCHAIN="$expect_keychain" KEYCHAIN="$keychain" \
EXPECT_HISTORY="$expect_history" EXPECT_DATABASE="$expect_database" EXPECT_CONTROL="$expect_control" \
EXPECT_AGENT="$expect_agent" CONNECT="$connect" BIND="$bind" EXPECT_BIND="$expect_bind" \
HOST_SOCK="$host_sock" LISTED_SOCK="$listed_sock" EXPECT_SSH_AGENT="$expect_ssh_agent" \
EXPECT_SSH_PUB="$expect_ssh_pub" SSH_PUB="$ssh_pub" \
EXPECT_SETTINGS_TARGET="$expect_settings_target" SETTINGS_TARGET="$settings_target" \
SETTINGS_FOLDER="$settings_folder" SETTINGS_MOVED="$settings_moved" \
OUTSIDE="$outside" AGENT_DIR="$agent_dir" STATE="$state" STATE_OTHER_CASE="$state_other_case" CONFIG_HOME="$config_home" PROBE="$probe" ERR="$scratch/err" \
    PITON_SANDBOX_POLICY="$policy" "$piton" sandbox-exec -- sh "$scratch/attempts.sh" </dev/null \
    >"$scratch/out" 2>&1 || status=$?
cat "$scratch/out"
# A move the sandbox let through and didn't undo: put the folder back.
if [ -n "$settings_folder" ] && [ -e "$settings_moved" ] && [ ! -e "$settings_folder" ]; then
    mv "$settings_moved" "$settings_folder"
fi
if [ "$status" -ne 0 ]; then
    echo "FAIL  the sandbox didn't start (piton sandbox-exec exited $status); nothing was tested" >&2
    exit 1
fi
# The integrity of the protected file itself, checked from outside the sandbox.
if [ "$(cat "$src/.piton/config.toml")" = "$marker" ]; then echo "pass  ok      protected file unchanged"; else echo "FAIL  changed protected file changed"; fi
# The named socket, checked from outside the sandbox: still the host's listener, still in its folder.
if [ -S "$listed_sock" ] && python3 -I "$connect" "$listed_sock" 2>/dev/null; then echo "pass  ok      named socket in place"; else echo "FAIL  moved   named socket moved"; fi
! rg -q '^FAIL' "$scratch/out"
