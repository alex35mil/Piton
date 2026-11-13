#!/bin/sh
# Host-only check of the protected-file Seatbelt rules (docs/design.md, "Protected files"): runs a shell in a nono
# sandbox with the rules the daemon generates (`piton config protected --seatbelt-rules`), in a scratch checkout, and
# reports what each write attempt did. Run from a host shell (not a sandboxed session: nono doesn't nest), after
# `mise run daemon:build`: `sh daemon/tests/probe/protected.sh`. Prints one line per attempt and exits 1 when any
# result differs from the expectation.
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

# The daemon's own rules for this checkout, as a JSON array.
rules=$("$piton" config protected --workspace "$src" --seatbelt-rules | python3 -I -c 'import json,sys; print(json.dumps([l.rstrip("\n") for l in sys.stdin if l.strip()]))')
profile="$scratch/profile.json"
cat > "$profile" <<EOF
{
  "meta": {"name": "piton-protected-probe"},
  "extends": "default",
  "workdir": {"access": "readwrite"},
  "filesystem": {"allow": ["$outside"]},
  "unsafe_macos_seatbelt_rules": $rules
}
EOF

# Each attempt: expectation (ok | denied), a description, and a shell command run in the sandbox from the checkout.
cat > "$scratch/attempts.sh" <<'EOF'
attempt() {
    expect=$1; what=$2; shift 2
    # No terminal: a tool that can't write asks before giving up (`rm`: "override rw-r--r--?") and would wait.
    if sh -c "$*" </dev/null >/dev/null 2>&1; then got=ok; else got=denied; fi
    if [ "$got" = "$expect" ]; then mark=pass; else mark=FAIL; fi
    printf '%-5s %-7s %s\n' "$mark" "$got" "$what"
}
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
attempt denied "create a skill in a subfolder"          'mkdir -p apps/.agents/skills/x'
attempt denied "edit an existing skill"                 'echo more >> .agents/skills/s/SKILL.md'
attempt denied "write .PI/ (another case)"              'mkdir -p .PI && echo x > .PI/settings.json'
attempt denied "write .Agents/Skills/ (another case)"   'mkdir -p .Agents/Skills/y && echo x > .Agents/Skills/y/SKILL.md'
attempt denied "write .PITON/ (another case)"           'echo x > .PITON/config.toml'
attempt ok     "write workflow artifacts"               'echo z > .piton/.local/x'
attempt ok     "write .piton/.LOCAL (another case)"     'echo z > .piton/.LOCAL/y'
attempt ok     "write a source file"                    'echo w > src/a.rs'
attempt ok     "write rules (not protected)"            'mkdir -p .agents/rules && echo r > .agents/rules/r.md'
attempt ok     "create a folder named like .pi but not" 'mkdir -p apps/.pix && echo q > apps/.pix/f'
# Moving a parent of a not-yet-existing protected folder is an ordinary write; the daemon guards its own writes
# against it (it never follows a symlink on an approved path).
attempt ok     "swap a parent folder for a symlink"     "mv apps apps.bak && ln -s $OUTSIDE apps"
EOF

cd "$src"
OUTSIDE="$outside" nono run -s --profile "$profile" --allow-cwd -- sh "$scratch/attempts.sh" </dev/null | tee "$scratch/out"
# The integrity of the protected file itself, checked from outside the sandbox.
if [ "$(cat "$src/.piton/config.toml")" = '{}' ]; then echo "pass  ok      protected file unchanged"; else echo "FAIL  changed protected file changed"; fi
! rg -q '^FAIL' "$scratch/out"
