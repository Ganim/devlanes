#!/bin/sh
# Claude Code PreToolUse hook for the Bash tool. Rejects commands that start a
# dev server in the background inside a devlanes project and points to
# `devlanes exec`. Exit 2 blocks the call and shows stderr to Claude; exit 0
# leaves the normal permission flow untouched. DEVLANES_BLOCK_BACKGROUND=0 in
# Claude Code's environment turns the check off.
set -u

input=$(cat)
[ "${DEVLANES_BLOCK_BACKGROUND:-1}" = 0 ] && exit 0

if command -v jq >/dev/null 2>&1; then
	cmd=$(printf '%s' "$input" | jq -r '.tool_input.command // empty' 2>/dev/null) || cmd=$input
	background=$(printf '%s' "$input" | jq -r '.tool_input.run_in_background // false' 2>/dev/null) || background=false
	dir=$(printf '%s' "$input" | jq -r '.cwd // empty' 2>/dev/null) || dir=
else
	# Without jq the raw JSON is matched. A command then ends at a `"` and escaped
	# newlines appear as a literal \n; the background pattern accepts both after `&`.
	cmd=$input
	case $input in
	*'"run_in_background":true'* | *'"run_in_background": true'*) background=true ;;
	*) background=false ;;
	esac
	dir=
fi
[ -n "$dir" ] || dir=$PWD

# Only inside a devlanes project: elsewhere `devlanes exec` has no lane to attach to.
in_project=false
while [ -n "$dir" ]; do
	if [ -f "$dir/devlanes.yaml" ]; then
		in_project=true
		break
	fi
	parent=$(dirname "$dir")
	[ "$parent" = "$dir" ] && break
	dir=$parent
done
[ "$in_project" = true ] || exit 0

# Device paths such as /dev/null would otherwise match the `dev` script name.
words=$(printf '%s\n' "$cmd" | sed 's#/dev/[A-Za-z0-9_]*##g')

if printf '%s\n' "$words" | grep -Eq '(^|[;&|[:space:]])devlanes[[:space:]]'; then
	exit 0
fi

# A lone `&` that ends a command; `&&`, `>&`, `&>` and `|&` are not backgrounding.
if ! printf '%s\n' "$words" | grep -Eq '(^|[^&>|])&([[:space:];)\\"]|$)' && [ "$background" != true ]; then
	exit 0
fi

long_running='(^|[^[:alnum:]_.-])(dev|start|serve|server|preview|watch|storybook|vite|next|nuxt|astro|wrangler|tauri|nodemon|webpack-dev-server|uvicorn|gunicorn|flask|runserver|rails|http\.server|up)([^[:alnum:]_-]|$)'
if ! printf '%s\n' "$words" | grep -Eq "$long_running"; then
	exit 0
fi

cat >&2 <<'EOF'
Blocked by the devlanes plugin: this starts a long-running process in the background, where nothing supervises it and its logs are lost.
Use instead:
  devlanes start [UNIT...]               services defined in devlanes.yaml
  devlanes exec --name NAME -- COMMAND   any other long-running command
Then check it with `devlanes wait NAME --timeout 180s` and `devlanes logs NAME --tail 100`.
EOF
exit 2
