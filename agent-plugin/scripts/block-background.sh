#!/bin/sh
# Claude Code PreToolUse hook for the Bash tool. Rejects commands that start a
# dev server in the background inside a devlanes project and points to
# `devlanes exec`. Exit 2 blocks the call and shows stderr to Claude; exit 0
# leaves the normal permission flow untouched. DEVLANES_BLOCK_BACKGROUND=0 in
# Claude Code's environment turns the check off. Tests: test-block-background.sh.
set -u

input=$(cat)
[ "${DEVLANES_BLOCK_BACKGROUND:-1}" = 0 ] && exit 0

# Prints the string value of a top-level-looking JSON key, unescaped; used without jq.
# shellcheck disable=SC2016 # an awk program; awk expands its $ fields
json_string='
BEGIN { RS = "\001" }
{
	p = index($0, "\"" key "\"")
	if (p == 0) exit
	s = substr($0, p + length(key) + 2)
	sub(/^[ \t\r\n]*:[ \t\r\n]*/, "", s)
	if (substr(s, 1, 1) != "\"") exit
	out = ""
	for (i = 2; i <= length(s); i++) {
		c = substr(s, i, 1)
		if (c == "\"") break
		if (c == "\\") {
			i++
			c = substr(s, i, 1)
			if (c == "n") c = "\n"
			else if (c == "t") c = "\t"
			else if (c == "r") c = ""
		}
		out = out c
	}
	printf "%s", out
}'

if command -v jq >/dev/null 2>&1; then
	cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // empty' 2>/dev/null) || cmd=
	background=$(printf '%s' "$input" | jq -r '.tool_input.run_in_background // false' 2>/dev/null) || background=false
	dir=$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null) || dir=
else
	cmd=$(printf '%s' "$input" | awk -v key=command "$json_string")
	dir=$(printf '%s' "$input" | awk -v key=cwd "$json_string")
	case $input in
	*'"run_in_background":true'* | *'"run_in_background": true'*) background=true ;;
	*) background=false ;;
	esac
fi
[ -n "$dir" ] || dir=$PWD

# Splits the command into simple commands, one per line as TAG<tab>text. Commands
# joined by &&, || or | share the tag of the operator that ends their list: BG for
# a lone &, FG otherwise. Operators inside quotes and redirections (2>&1, &>) don't split.
# shellcheck disable=SC2016 # an awk program
split='
BEGIN { RS = "\001"; n = 0 }
function add() {
	gsub(/^[ \t({]+|[ \t)}]+$/, "", seg)
	if (seg != "") list[++n] = seg
	seg = ""
}
function flush(tag,   k) {
	add()
	for (k = 1; k <= n; k++) printf "%s\t%s\n", tag, list[k]
	n = 0
}
{
	s = $0; seg = ""; q = ""
	for (i = 1; i <= length(s); i++) {
		c = substr(s, i, 1)
		if (q == "\047") { seg = seg c; if (c == "\047") q = ""; continue }
		if (c == "\\") { seg = seg c substr(s, i + 1, 1); i++; continue }
		if (q == "\"") { seg = seg (c == "\n" ? " " : c); if (c == "\"") q = ""; continue }
		if (c == "\047" || c == "\"") { q = c; seg = seg c; continue }
		nx = substr(s, i + 1, 1); pv = substr(s, i - 1, 1)
		if (c == "&") {
			if (nx == "&") { add(); i++; continue }
			if (nx == ">" || pv == ">" || pv == "<") { seg = seg c; continue }
			flush("BG"); continue
		}
		if (c == "|") { if (nx == "|" || nx == "&") i++; add(); continue }
		if (c == ";" || c == "\n") { flush("FG"); continue }
		seg = seg c
	}
	flush("FG")
}'

# devlanes exec and devlanes start only make sense inside a devlanes project.
in_project() {
	d=$1
	while [ -n "$d" ]; do
		[ -f "$d/devlanes.yaml" ] && return 0
		parent=$(dirname "$d")
		[ "$parent" = "$d" ] && return 1
		d=$parent
	done
	return 1
}

# Words that name a long-running script or command: pnpm dev, npm start, docker compose up.
long_running() {
	for word in "$@"; do
		word=${word#[\"\']}
		word=${word%[\"\']}
		case $word in
		dev | start | serve | server | preview | watch | --watch | storybook | runserver | up | http.server) return 0 ;;
		*server.js | *server.mjs | *server.ts | *server.py) return 0 ;;
		esac
	done
	case ${1##*/} in
	vite | next | nuxt | astro | wrangler | nodemon | webpack-dev-server | uvicorn | gunicorn | flask | hugo | http-server | live-server)
		for word in "$@"; do
			case $word in build | test | lint | check) return 1 ;; esac
		done
		return 0
		;;
	esac
	return 1
}

tab=$(printf '\t')
segments=$(printf '%s' "$cmd" | awk "$split")
set -f
while IFS="$tab" read -r tag segment; do
	# shellcheck disable=SC2086 # word splitting is the point
	set -- $segment
	[ "$#" -gt 0 ] || continue
	while [ "$#" -gt 0 ]; do
		case $1 in
		[A-Za-z_]*=*) shift ;;
		nohup | exec | command | time | env) shift ;;
		*) break ;;
		esac
	done
	[ "$#" -gt 0 ] || continue

	if [ "$1" = cd ]; then
		target=${2:-$HOME}
		target=${target#[\"\']}
		target=${target%[\"\']}
		case $target in
		-) ;;
		\~) dir=$HOME ;;
		\~/*) dir=$HOME/${target#\~/} ;;
		/*) dir=$target ;;
		*) dir=$dir/$target ;;
		esac
		resolved=$(cd "$dir" 2>/dev/null && pwd) && dir=$resolved
		continue
	fi

	detached=false
	if [ "$tag" = BG ] || [ "$background" = true ]; then detached=true; fi
	case $1 in
	setsid)
		shift
		while [ "$#" -gt 0 ]; do
			case $1 in
			--fork | -*f*) detached=true; shift ;;
			-*) shift ;;
			*) break ;;
			esac
		done
		;;
	tmux)
		case " $* " in *" new "* | *" new-session "*)
			for word in "$@"; do
				case $word in --*) ;; -*d*) detached=true ;; esac
			done
			;;
		esac
		;;
	screen)
		flags=
		for word in "$@"; do case $word in -*) flags=$flags$word ;; esac; done
		case $flags in *d*m* | *m*d*) detached=true ;; esac
		;;
	esac
	[ "$detached" = true ] || continue
	[ "$#" -gt 0 ] || continue
	# The backgrounded command is devlanes itself, such as devlanes logs -f.
	[ "${1##*/}" = devlanes ] && continue
	long_running "$@" || continue
	in_project "$dir" || continue

	cat >&2 <<'EOF'
Blocked by the devlanes plugin: this starts a long-running process in the background, where nothing supervises it and its logs are lost.
Use instead:
  devlanes start [UNIT...]               services defined in devlanes.yaml
  devlanes exec --name NAME -- COMMAND   any other long-running command
Then check it with `devlanes wait NAME --timeout 180s` and `devlanes logs NAME --tail 100`.
EOF
	exit 2
done <<EOF
$segments
EOF
exit 0
