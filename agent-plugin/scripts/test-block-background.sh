#!/bin/sh
# Tests block-background.sh with and without jq. Exits 1 if any verdict is wrong.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
hook=$here/block-background.sh
work=$(mktemp -d)
cleanup() { rm -rf "$work"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

mkdir -p "$work/project/sub" "$work/other" "$work/nojq"
touch "$work/project/devlanes.yaml"
# The hook's tools minus jq, for the raw-JSON path.
for tool in cat awk dirname; do ln -s "$(command -v "$tool")" "$work/nojq/$tool"; done

# Escapes a shell string as a JSON string body.
json() {
	printf '%s' "$1" | awk 'BEGIN { RS = "\001" } {
		gsub(/\\/, "\\\\"); gsub(/"/, "\\\""); gsub(/\t/, "\\t"); gsub(/\n/, "\\n"); printf "%s", $0 }'
}

failures=0
# check EXPECTED DIR COMMAND [RUN_IN_BACKGROUND]
check() {
	expected=$1 dir=$2 command=$3 bg=${4:-false}
	payload=$(printf '{"session_id":"s","cwd":"%s","tool_name":"Bash","tool_input":{"command":"%s","description":"runs \\"command\\": x","run_in_background":%s}}' \
		"$(json "$dir")" "$(json "$command")" "$bg")
	modes=nojq
	if command -v jq >/dev/null 2>&1; then modes="jq nojq"; fi
	for mode in $modes; do
		status=0
		if [ "$mode" = jq ]; then
			(cd "$work" && printf '%s' "$payload" | sh "$hook" 2>/dev/null) || status=$?
		else
			(cd "$work" && printf '%s' "$payload" | PATH="$work/nojq" /bin/sh "$hook" 2>/dev/null) || status=$?
		fi
		verdict=allow
		if [ "$status" = 2 ]; then verdict=block; fi
		if [ "$verdict" != "$expected" ]; then
			printf 'FAIL (%s): expected %s in %s: %s\n' "$mode" "$expected" "$dir" "$command"
			failures=$((failures + 1))
		fi
	done
}

p=$work/project/sub
o=$work/other

check block "$p" 'pnpm dev &'
check block "$p" 'npm run dev &'
check block "$p" 'nohup pnpm dev > dev.log 2>&1 &'
check block "$p" 'pnpm dev &>/dev/null &'
check block "$p" 'npm start & sleep 5 && curl localhost:3000'
check block "$p" 'pnpm dev | tee dev.log &'
check block "$p" 'python -m http.server 8000 &'
check block "$p" 'node server.js &'
check block "$p" 'vite --port 5173 &'
check block "$p" 'pnpm dev & disown'
check block "$p" 'setsid -f pnpm dev'
check block "$p" 'tmux new -d "pnpm dev"'
check block "$p" 'tmux new-session -ds web npm start'
check block "$p" 'screen -dmS web pnpm dev'
check block "$p" 'PORT=3001 pnpm dev &'
check block "$p" 'pnpm dev
sleep 3' true
check block "$o" "cd $work/project && pnpm dev &"
check block "$o" "cd ../project/sub && pnpm dev &"
check block "$p" 'echo "a & b" && pnpm dev &'

check allow "$p" 'pnpm dev'
check allow "$p" 'pnpm build && pnpm test'
check allow "$p" 'pnpm test' true
check allow "$p" 'sleep 10 >/dev/null &'
check allow "$p" 'tail -f dev.log &'
check allow "$p" 'tail -f /var/log/server.log &'
check allow "$p" 'echo "pnpm dev &"'
check allow "$p" "echo 'npm start &'"
check allow "$p" 'curl "https://dev.example.invalid/?a=1&b=2"'
check allow "$p" 'cargo build 2>&1 | tail'
check allow "$p" 'pnpm dev &>/dev/null'
check allow "$p" 'vite build &'
check allow "$p" 'setsid pnpm dev'
check allow "$p" 'devlanes exec --name web -- pnpm dev'
check allow "$p" 'devlanes logs web -f --timeout 30s &'
check allow "$p" 'devlanes start' true
check allow "$o" 'pnpm dev &'
check allow "$p" "cd $work/other && pnpm dev &"
DEVLANES_BLOCK_BACKGROUND=0 check allow "$p" 'pnpm dev &'

if [ "$failures" -gt 0 ]; then
	printf '%s failure(s)\n' "$failures"
	exit 1
fi
echo "block-background.sh: all cases pass"
