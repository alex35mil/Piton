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
probe="piton-probe-$$"
agent_dir=${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}
# Outside every write the sandbox has, and not the home folder: /private/var/tmp is no profile's write root. One folder
# per run. A run killed with SIGKILL skips its trap, so earlier runs' folders older than a day are removed first, and so
# are the home entries a run names after its pid (they exist only when the sandbox let a denied write through) whose
# run is gone.
find /private/var/tmp -maxdepth 1 -type d -name 'piton-test.*' -mtime +0 -exec rm -rf {} + 2>/dev/null || true
for stale in "$HOME"/piton-probe-* "$HOME/Library/Caches"/piton-probe-* "$agent_dir/extensions"/piton-probe-*; do
    test -e "$stale" || continue
    pid=${stale##*/piton-probe-}
    pid=${pid%%[!0-9]*}
    test -n "$pid" && kill -0 "$pid" 2>/dev/null || rm -rf "$stale"
done
away=$(mktemp -d /private/var/tmp/piton-test.XXXXXX)
# A grant through a link the sandbox could have made (here, in the checkout) mustn't reach the link's target.
linked="$away/linked"
# Names for the entries the denied attempts would create in places that can only be checked where they are (the home
# folder, a user cache, pi's extensions); removed below if they exist.
cleanup_real() {
    rm -rf "$away"
    rm -f "$HOME/$probe"
    rmdir "$HOME/Library/Caches/$probe" "$agent_dir/extensions/$probe" 2>/dev/null || true
    rmdir "$agent_dir/TRUST.JSON.LOCK" 2>/dev/null || true
}
listener=
# The listener is waited for after it's killed, so the shell doesn't report its end ("Terminated: 15").
# The script's exit status is kept: the killed listener's wait (143) would replace it, and under `set -e` end the trap
# early.
trap 'code=$?; test -z "$listener" || { kill "$listener" 2>/dev/null; wait "$listener" 2>/dev/null || true; }; cleanup_real; chmod -R u+w "$scratch" 2>/dev/null; rm -rf "$scratch"; exit $code' EXIT
mkdir "$linked"
ln -s "$linked" "$src/cache"
marker="# piton protected-file probe
[sandbox.filesystem]
\"$src/cache\" = \"write\""
printf '%s\n' "$marker" > "$src/.piton/config.toml"
# Unix sockets the host listens on, in the scratch folder (temp, which the sandbox writes): one no entry names, and one
# the checkout's own config.local.toml lists, which must stay in place.
mkdir -p "$scratch/host" "$scratch/listed"
host_sock="$scratch/host/listen.sock"
listed_sock="$scratch/listed/ok.sock"
printf '[sandbox.network.unix_socket_connections]\n"%s" = "allow"\n' "$listed_sock" > "$src/.piton/config.local.toml"
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

# The daemon's policy for this checkout, with the user's own global configuration (`$XDG_CONFIG_HOME/piton`, else
# ~/.config/piton).
if ! shown=$("$piton" sandbox show --workspace "$src"); then
    echo "piton sandbox show failed: fix the configuration it names, then run this again" >&2
    exit 1
fi
policy=$(printf '%s' "$shown" | python3 -I -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["policy"]))')
state=$HOME/.local/state/piton
state_other_case=$HOME/.LOCAL/STATE/PITON
config_home=${XDG_CONFIG_HOME:-$HOME/.config}/piton

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
# A Mach service lookup: exits 0 when found, 1 when the sandbox refuses it (1100), 2 when no such service runs (1102).
lookup="$scratch/lookup.py"
cat > "$lookup" <<'PY'
import ctypes, sys
lib = ctypes.CDLL("/usr/lib/libSystem.dylib")
port = ctypes.c_uint(0)
kr = lib.bootstrap_look_up(ctypes.c_uint.in_dll(lib, "bootstrap_port").value, sys.argv[1].encode(), ctypes.byref(port))
sys.exit({0: 0, 1100: 1}.get(kr, 2))
PY
# What `open` and `osascript` would start on the host: a script that leaves a marker, checked from outside.
launched="$away/launched"
printf '#!/bin/sh\ntouch "%s"\n' "$launched" > "$src/x.command"
chmod +x "$src/x.command"
expect_pasteboard=$(policy_says --service com.apple.pasteboard.1)
expect_apps=$("$piton" sandbox show --workspace "$src" | python3 -I -c 'import json,sys; print("ok" if json.load(sys.stdin)["policy"]["apps"]["kind"] == "open" else "denied")')
expect_bind=$(policy_says --unix-bind "$src/probe.sock")
# TCP listening, as the policy decides it (the default profile and a pi-claude-code preset open it; every address alike).
listen_of() { python3 -I -c 'import json,sys; print("ok" if json.load(sys.stdin)["listen"]["kind"] == "open" else "denied")'; }
expect_listen=$(printf '%s' "$policy" | listen_of)
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
# Denied by default; allowed when the configuration has a `"read"` entry for the keychains in `[sandbox.filesystem]`.
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
attempt "$EXPECT_LISTEN" "listen on TCP 127.0.0.1"   "python3 -I -c 'import socket; s=socket.socket(); s.bind((\"127.0.0.1\", 0)); s.listen()'"
attempt "$EXPECT_LISTEN" "listen on TCP 0.0.0.0"     "python3 -I -c 'import socket; s=socket.socket(); s.bind((\"0.0.0.0\", 0)); s.listen()'"
attempt denied "connect to a host process's socket"    "python3 -I $CONNECT $HOST_SOCK"
attempt denied "connect to it through a hard link"     "ln $HOST_SOCK hl.sock && python3 -I $CONNECT hl.sock"
attempt ok     "connect to a socket an entry names"    "python3 -I $CONNECT $LISTED_SOCK"
attempt denied "move another socket onto a named one"  "mv $LISTED_SOCK $OUTSIDE/moved.sock || true; ln -f $HOST_SOCK $LISTED_SOCK"
attempt denied "rename a named socket's folder"        "mv $(dirname $LISTED_SOCK) $OUTSIDE/moved-folder"
attempt "$EXPECT_BIND" "create a socket in the workspace" "python3 -I $BIND probe.sock"
attempt "$EXPECT_SSH_AGENT" "list the SSH agent's keys" "ssh-add -l"
attempt "$EXPECT_SSH_PUB" "read an SSH public key"    "cat '$SSH_PUB'"
# Mach services and apps: the keychain's closed; the rest as the profile decides (standard: every service).
attempt ok     "look up a built-in service (fonts)"    "python3 -I $LOOKUP com.apple.fonts"
attempt "$EXPECT_KEYCHAIN" "look up the keychain's service" "python3 -I $LOOKUP com.apple.SecurityServer"
attempt "$EXPECT_PASTEBOARD" "look up the pasteboard's service" "python3 -I $LOOKUP com.apple.pasteboard.1"
attempt "$EXPECT_APPS" "open a script the sandbox wrote"   "open -g x.command"
attempt "$EXPECT_APPS" "open an app"                       "open -g -a Calculator"
attempt "$EXPECT_APPS" "tell Terminal to run a command"    "osascript -e 'tell application \"Terminal\" to do script \"touch $LAUNCHED\"'"
EOF

cd "$src"
status=0
EXPECT_SSH_KEY="$expect_ssh_key" SSH_KEY="$ssh_key" EXPECT_KEYCHAIN="$expect_keychain" KEYCHAIN="$keychain" \
EXPECT_HISTORY="$expect_history" EXPECT_DATABASE="$expect_database" EXPECT_CONTROL="$expect_control" \
EXPECT_AGENT="$expect_agent" CONNECT="$connect" BIND="$bind" EXPECT_BIND="$expect_bind" EXPECT_LISTEN="$expect_listen" \
HOST_SOCK="$host_sock" LISTED_SOCK="$listed_sock" EXPECT_SSH_AGENT="$expect_ssh_agent" \
EXPECT_SSH_PUB="$expect_ssh_pub" SSH_PUB="$ssh_pub" \
LOOKUP="$lookup" EXPECT_PASTEBOARD="$expect_pasteboard" EXPECT_APPS="$expect_apps" LAUNCHED="$launched" \
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
# Nothing `open` or `osascript` tried started on the host, unless the policy launches apps.
sleep 2
if [ -e "$launched" ] && [ "$expect_apps" = denied ]; then
    echo "FAIL  ran     a launched app ran the sandbox's script" | tee -a "$scratch/out"
else
    echo "pass  ok      no app ran the sandbox's script"
fi
quit_calculator() { osascript -e 'tell application "Calculator" to quit' >/dev/null 2>&1 || true; }
test "$expect_apps" = denied || quit_calculator

# The strict profile: basic tools still work with only the built-in Mach services; another app's service is refused.
# Run in a second checkout whose config.local.toml picks strict (loosening keys: this machine's own file).
strict="$scratch/strict"
mkdir -p "$strict/.piton/.local"
git -C "$strict" init -q
# strict's network is filtered: the hosts these checks reach are listed, plus a local port and a port on any host. The
# gate runs for the pass (`piton sandbox gate`), deciding by these entries and pi's own hosts.
local_port=$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
other_port=$(python3 -I -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
printf '[sandbox]\nprofile = "strict"\n[sandbox.network.connections]\n"github.com" = "allow"\n"example.com" = "allow"\n"localhost:%s" = "allow"\n"*:7" = "allow"\n' "$local_port" \
    > "$strict/.piton/config.local.toml"
mkfifo "$scratch/gate-in"
("$piton" sandbox gate --workspace "$strict" < "$scratch/gate-in" > "$scratch/gate-policy" 2>"$scratch/gate-err") &
gate_pid=$!
exec 9> "$scratch/gate-in"
for _ in $(seq 50); do test -s "$scratch/gate-policy" && break; sleep 0.1; done
strict_policy=$(head -n 1 "$scratch/gate-policy")
# A preset of this machine's own config can grant every local port (pi-claude-code's `localhost:*`): then any local
# port connects.
other_local=denied
printf '%s' "$strict_policy" | rg -q '"kind":"local_any_port"' && other_local=ok
# The same preset opens TCP listening (its relay listens): what strict's listens expect here.
strict_listen=$(printf '%s' "$strict_policy" | listen_of)
listeners=""
for port in "$local_port" "$other_port"; do
    # Without fd 9: a listener holding the gate's FIFO open would keep the gate from ever seeing it close.
    python3 -I -c 'import socket,sys; s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(("127.0.0.1", int(sys.argv[1]))); s.listen(8)
while True: s.accept()[0].close()' "$port" >/dev/null 2>&1 9>&- &
    listeners="$listeners $!"
done
cat > "$scratch/strict.sh" <<'EOF'
attempt() {
    expect=$1; what=$2; shift 2
    if sh -c "$*" </dev/null >/dev/null 2>"$ERR"; then got=ok; else got=denied; fi
    if [ "$got" = "$expect" ]; then mark=pass; else mark=FAIL; fi
    printf '%-5s %-7s strict: %s\n' "$mark" "$got" "$what"
    # The error's first lines: the cause comes first (node ends with its version banner).
    if [ "$mark" = FAIL ] && [ -s "$ERR" ]; then head -n 12 "$ERR" | sed 's/^/      /'; fi
}
attempt ok     "git status"                               'git status'
attempt ok     "git over HTTPS (TLS)"                     'git ls-remote https://github.com/git/git HEAD'
attempt ok     "curl over HTTPS"                          'curl -sfI https://example.com'
attempt ok     "python TLS (urllib, through the gate)"    "python3 -I -c 'import urllib.request; urllib.request.urlopen(\"https://github.com\", timeout=10).close()'"
attempt ok     "user name (id)"                           'id -un'
attempt ok     "read a preference (defaults)"            'defaults read -g AppleLocale'
attempt ok     "look up a built-in service (logd)"       "python3 -I $LOOKUP com.apple.logd"
attempt denied "look up the pasteboard's service"        "python3 -I $LOOKUP com.apple.pasteboard.1"
attempt denied "open a link"                             'open -g https://example.com'
# The filtered network: listed hosts through the gate, the rest refused; ports by the sandbox itself.
attempt ok     "curl http:// to a listed host (gate)"   'curl -sf -o /dev/null http://example.com'
attempt denied "curl to an unlisted host (gate)"         'curl -sf -o /dev/null --max-time 10 https://www.apple.com'
attempt denied "TCP straight to a host, past the gate"   "python3 -I -c 'import socket; socket.create_connection((\"93.184.215.14\", 443), 5)'"
attempt ok     "a listed local port (localhost:PORT)"   "python3 -I -c 'import socket; socket.create_connection((\"127.0.0.1\", $LOCAL_PORT), 5)'"
attempt $OTHER_LOCAL "another local port"               "python3 -I -c 'import socket; socket.create_connection((\"127.0.0.1\", $OTHER_PORT), 5)'"
attempt denied "the gate without its credential"         "curl -sf -o /dev/null --max-time 10 -x http://127.0.0.1:\${HTTPS_PROXY##*:} https://example.com"
attempt ok     "git over SSH to a listed host (wrapper)" 'ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -T git@github.com 2>&1 | rg -q "successfully authenticated|Permission denied"'
attempt $STRICT_LISTEN "listen on TCP 127.0.0.1"         "python3 -I -c 'import socket; s=socket.socket(); s.bind((\"127.0.0.1\", 0)); s.listen()'"
attempt $STRICT_LISTEN "listen on TCP 0.0.0.0"           "python3 -I -c 'import socket; s=socket.socket(); s.bind((\"0.0.0.0\", 0)); s.listen()'"
EOF
# Node's TLS: a script file, so no quoting nests (`echo` treats backslashes differently across shells).
cat > "$scratch/fetch.mjs" <<'JS'
const r = await fetch("https://example.com").catch(e => { console.error(e.cause ?? e); process.exit(1) })
process.exit(r.ok ? 0 : 1)
JS
if command -v node >/dev/null 2>&1; then
    echo "attempt ok \"node fetch over HTTPS\" 'node $scratch/fetch.mjs'" >> "$scratch/strict.sh"
fi
(cd "$strict" && LOOKUP="$lookup" ERR="$scratch/err" LOCAL_PORT="$local_port" OTHER_PORT="$other_port" OTHER_LOCAL="$other_local" \
    STRICT_LISTEN="$strict_listen" PITON_SANDBOX_POLICY="$strict_policy" \
    "$piton" sandbox-exec -- sh "$scratch/strict.sh" </dev/null >"$scratch/strict-out" 2>&1 9>&-) \
    || echo "FAIL  the strict sandbox didn't start" >> "$scratch/strict-out"
# Closed listening, whatever this machine's presets open: strict's policy with listening closed, and creating sockets
# opened in temp (strict closes it) for the Unix line. Every TCP listen is refused, on loopback too; a UDP bind (DNS
# replies), a Unix socket listen, and a TCP connect still work.
closed_policy=$(printf '%s' "$strict_policy" | python3 -I -c '
import json, sys
p = json.load(sys.stdin)
p["listen"] = {"kind": "closed"}
p["rules"].insert(1, {"effect": "allow", "access": ["bind"], "target": {"kind": "subpath", "path": "/private/tmp"},
                      "source": {"kind": "workspace"}})
print(json.dumps(p))')
cat > "$scratch/closed.sh" <<'EOF'
attempt() {
    expect=$1; what=$2; shift 2
    if sh -c "$*" </dev/null >/dev/null 2>"$ERR"; then got=ok; else got=denied; fi
    if [ "$got" = "$expect" ]; then mark=pass; else mark=FAIL; fi
    printf '%-5s %-7s closed listening: %s\n' "$mark" "$got" "$what"
    if [ "$mark" = FAIL ] && [ -s "$ERR" ]; then head -n 12 "$ERR" | sed 's/^/      /'; fi
}
attempt denied "listen on TCP 127.0.0.1"     "python3 -I -c 'import socket; s=socket.socket(); s.bind((\"127.0.0.1\", 0)); s.listen()'"
attempt denied "listen on TCP 0.0.0.0"       "python3 -I -c 'import socket; s=socket.socket(); s.bind((\"0.0.0.0\", 0)); s.listen()'"
attempt denied "listen on TCP :: (dual-stack)" "python3 -I -c 'import socket; s=socket.socket(socket.AF_INET6); s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0); s.bind((\"::\", 0)); s.listen()'"
attempt ok     "bind UDP on 0.0.0.0"         "python3 -I -c 'import socket; socket.socket(socket.AF_INET, socket.SOCK_DGRAM).bind((\"0.0.0.0\", 0))'"
attempt ok     "look up a host name (DNS)"   "python3 -I -c 'import socket; socket.getaddrinfo(\"github.com\", 443)'"
attempt ok     "listen on a Unix socket"     "python3 -I -c 'import socket,os; p=\"/private/tmp/piton-probe-\"+str(os.getpid())+\".sock\"; s=socket.socket(socket.AF_UNIX); s.bind(p); s.listen(); os.unlink(p)'"
attempt ok     "connect to a listed local port" "python3 -I -c 'import socket; socket.create_connection((\"127.0.0.1\", $LOCAL_PORT), 5)'"
EOF
(cd "$strict" && ERR="$scratch/err" LOCAL_PORT="$local_port" PITON_SANDBOX_POLICY="$closed_policy" \
    "$piton" sandbox-exec -- sh "$scratch/closed.sh" </dev/null >>"$scratch/strict-out" 2>&1 9>&-) \
    || echo "FAIL  the closed-listening sandbox didn't start" >> "$scratch/strict-out"
# A killed listener's wait returns 143, which `set -e` would take for a failure.
for l in $listeners; do kill "$l" 2>/dev/null || true; wait "$l" 2>/dev/null || true; done
# Closing the FIFO stops the gate.
exec 9>&-
wait "$gate_pid" 2>/dev/null || true
# Shown whole, so a failed line keeps the error line below it.
cat "$scratch/strict-out"
cat "$scratch/strict-out" >> "$scratch/out"

# Launching apps: a link opens in the browser (from a strict checkout whose config.local.toml sets
# allow_launching_apps, so the services that switch opens are what open uses).
apps="$scratch/apps"
mkdir -p "$apps/.piton/.local"
git -C "$apps" init -q
printf '[sandbox]\nprofile = "strict"\nallow_launching_apps = true\n' > "$apps/.piton/config.local.toml"
apps_policy=$("$piton" sandbox show --workspace "$apps" | python3 -I -c 'import json,sys; print(json.dumps(json.load(sys.stdin)["policy"]))')
if (cd "$apps" && PITON_SANDBOX_POLICY="$apps_policy" "$piton" sandbox-exec -- open -g https://example.com </dev/null >/dev/null 2>"$scratch/err"); then
    echo "pass  ok      strict + allow_launching_apps: open a link" | tee -a "$scratch/out"
else
    echo "FAIL  denied  strict + allow_launching_apps: open a link" | tee -a "$scratch/out"
    printf '      %s\n' "$(tail -n 1 "$scratch/err")"
fi
# Every refusal reaches its call: with a tag, the default deny carries it too, so the kernel's record of a refusal no
# rule names (a preferences write, a launch, a signal to another process) carries the launch's tag (#357).
tag="probe$$"
tagged_policy=$(printf '%s' "$policy" | python3 -I -c "import json,sys; p=json.load(sys.stdin); p['tag']='$tag'; print(json.dumps(p))")
tagged_start=$(date '+%Y-%m-%d %H:%M:%S')
# Another process of this user, outside the sandbox: the sandboxed shell sends it SIGTERM (refused; `kill -0` is refused
# too, but leaves no record).
sleep 300 & other=$!
(cd "$src" && PITON_SANDBOX_POLICY="$tagged_policy" "$piton" sandbox-exec -- sh -c "
    defaults write com.piton.probe$$ k v; open -g https://example.com; kill -TERM $other" </dev/null >/dev/null 2>&1) || true
kill "$other" 2>/dev/null || true
wait "$other" 2>/dev/null || true
defaults delete "com.piton.probe$$" >/dev/null 2>&1 || true
sleep 2
records=$(log show --start "$tagged_start" --style ndjson \
    --predicate "senderImagePath ENDSWITH \"/Sandbox\" AND eventMessage CONTAINS \"piton-$tag\"" 2>/dev/null)
for want in "user-preference-write com.piton.probe$$|a preferences write" "lsopen|open a link" "signal |a signal to another process"; do
    op=${want%%|*}; what=${want#*|}
    if printf '%s' "$records" | rg -qF "deny(1) $op"; then
        echo "pass  tagged  $what is recorded with the launch's tag" | tee -a "$scratch/out"
    else
        echo "FAIL  untagged $what is recorded with the launch's tag" | tee -a "$scratch/out"
    fi
done
! rg -q '^FAIL' "$scratch/out"
