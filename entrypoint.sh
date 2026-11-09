#!/bin/bash
set -euo pipefail

HOME_DIR=/home/dev
WORKSPACE=$HOME_DIR/workspace
# outside the home volume on purpose: a recreated container loses setup.sh's apt packages, so the marker must go with them and setup.sh reruns
MARK_DIR=/var/lib/dockbench

if [ "${INSTALL_CLAUDE:-1}" = "1" ]; then
  # Claude Code lives in the home volume (~/.local), not the image: install it on the first start, update it to the latest release on every later start. A failure only warns, so an outage never stops the dev environment.
  if command -v claude >/dev/null; then
    timeout 120 claude update || echo "warning: claude update failed, keeping the installed Claude Code" >&2
  else
    timeout 300 bash -o pipefail -c 'curl -fsSL https://claude.ai/install.sh | bash' || echo "warning: could not install Claude Code" >&2
  fi
  # claude update keeps every replaced version (about 300 MB each) in the volume: keep only the one ~/.local/bin/claude points to
  versions=$HOME_DIR/.local/share/claude/versions
  current=$(readlink -f "$HOME_DIR/.local/bin/claude" || true)
  if [ -x "$current" ] && [ "$(dirname "$current")" = "$versions" ]; then
    for v in "$versions"/*; do
      [ "$v" = "$current" ] || rm -rf "$v" || echo "warning: could not remove old Claude Code version $v" >&2
    done
  fi
fi

git config --global user.name "$GIT_USER_NAME"
git config --global user.email "$GIT_USER_EMAIL"

case "$REPO_URL" in
  *://*|git@*) url=$REPO_URL ;;
  *) url=https://github.com/$REPO_URL.git ;;
esac
# origin follows REPO_URL, but a user who removed or renamed origin must not get a restart loop out of it
set_origin() { if [ -d "$WORKSPACE/.git" ]; then git -C "$WORKSPACE" remote set-url origin "$url" || echo "warning: could not point origin at $url" >&2; fi; }

if [ -n "${SSH_KEY:-}" ]; then
  # the dashboard passes a per-repo deploy key; rewrite it on every start so a redeploy with a new key takes effect
  mkdir -p "$HOME_DIR/.ssh" && chmod 700 "$HOME_DIR/.ssh"
  if [ -e "$HOME_DIR/.ssh/id_ed25519" ] && [ ! -e "$HOME_DIR/.ssh/id_ed25519.dockbench" ]; then
    echo "warning: ~/.ssh/id_ed25519 exists and is not ours (no .dockbench marker), keeping it instead of SSH_KEY" >&2
  else
    # umask so the file is never readable by others, not even before a chmod; a bad value must stop here with its name, not later as a bare "Permission denied (publickey)"
    (umask 077 && printf '%s' "$SSH_KEY" | base64 -d > "$HOME_DIR/.ssh/id_ed25519") || { echo "error: SSH_KEY is not valid base64" >&2; exit 1; }
    # the marker tells a later start that this key file is ours to delete, so a key the user put there by hand is never touched
    touch "$HOME_DIR/.ssh/id_ed25519.dockbench"
  fi
  # a workspace cloned over https by an older stack would keep using gh's account token: point it at the deploy key and drop that token
  set_origin
  gh auth logout --hostname github.com >/dev/null 2>&1 || true
else
  if [ -e "$HOME_DIR/.ssh/id_ed25519.dockbench" ]; then
    # the stack switched from the dashboard's deploy key to gh login: drop the dead key and move origin off its ssh URL, since gh setup-git only covers https
    rm -f "$HOME_DIR/.ssh/id_ed25519" "$HOME_DIR/.ssh/id_ed25519.dockbench"
    set_origin
  fi
  if [ -z "${GITHUB_TOKEN:-}" ] && ! gh auth status --hostname github.com >/dev/null 2>&1; then
    gh auth login --hostname github.com --git-protocol https --web
  fi
  gh auth setup-git --hostname github.com --force
fi

# the dashboard empties CLAUDE_CONFIG_KEY when the config is turned off and removes its deploy key: drop the private half the home volume still holds, only if this script wrote it
if [ -z "${CLAUDE_CONFIG_KEY:-}" ] && [ -e "$HOME_DIR/.ssh/claude_config.dockbench" ]; then rm -f "$HOME_DIR/.ssh/claude_config" "$HOME_DIR/.ssh/claude_config.dockbench"; fi

if [ -n "${CLAUDE_CONFIG_REPO:-}" ]; then
  # overlay the user's ~/.claude config repo on every start: tracked files are overwritten, untracked ones (credentials, history) are kept; fetching by URL keeps a token in the URL out of .git/config
  mkdir -p "$HOME_DIR/.claude"
  git -C "$HOME_DIR/.claude" init -q
  cfg=()
  if [ -n "${CLAUDE_CONFIG_KEY:-}" ]; then
    # the dashboard passes a read-only deploy key for the config repo; IdentitiesOnly keeps ssh from offering the workspace key, which GitHub would accept and then refuse for this repo
    mkdir -p "$HOME_DIR/.ssh" && chmod 700 "$HOME_DIR/.ssh"
    printf '%s' "$CLAUDE_CONFIG_KEY" | base64 -d > "$HOME_DIR/.ssh/claude_config" && chmod 600 "$HOME_DIR/.ssh/claude_config"
    touch "$HOME_DIR/.ssh/claude_config.dockbench"
    cfg=(-c "core.sshCommand=ssh -i $HOME_DIR/.ssh/claude_config -o IdentitiesOnly=yes")
  fi
  # timeout: a stalled connection would otherwise hang the start before the tunnel and the status server, with no error to show
  if timeout 60 git "${cfg[@]}" -C "$HOME_DIR/.claude" fetch -q --depth 1 "$CLAUDE_CONFIG_REPO" HEAD; then
    git -C "$HOME_DIR/.claude" reset -q --hard FETCH_HEAD
  else
    echo "warning: could not fetch CLAUDE_CONFIG_REPO, keeping the current ~/.claude" >&2
  fi
fi

if [ "${INSTALL_CLAUDE:-1}" = "1" ] && command -v claude >/dev/null; then
  # pre-accept Claude Code's trust prompt for the workspace (it also covers the folders below it), so the first claude there does not stop on it. The edit goes to a copy moved in only on success, so a failure never leaves a broken ~/.claude.json
  cj=$HOME_DIR/.claude.json
  if ! jq -e --arg w "$WORKSPACE" '.projects[$w].hasTrustDialogAccepted == true' "$cj" >/dev/null 2>&1; then
    if ! { { [ -f "$cj" ] || { echo '{}' > "$cj" && chmod 600 "$cj"; }; } && cp -p "$cj" "$cj.tmp" && jq --arg w "$WORKSPACE" '.projects[$w].hasTrustDialogAccepted = true' "$cj" > "$cj.tmp" && mv "$cj.tmp" "$cj"; }; then
      rm -f "$cj.tmp"
      echo "warning: could not pre-accept the Claude Code trust prompt for $WORKSPACE" >&2
    fi
  fi

  if [ -f "$HOME_DIR/.claude/settings.json" ]; then
    # install and update every plugin settings.json enables (skills, agents and hooks already come with CLAUDE_CONFIG_REPO), so a new container needs no manual /plugin. It runs in the background so its marketplace and plugin calls never delay the tunnel; /status reports its state and warnings from $MARK_DIR/claude-plugins.json. Every failure only warns.
    mkdir -p "$MARK_DIR"
    (
      warnings=()
      warn() { echo "warning: $1" >&2; warnings+=("$1"); }
      sync_state() {
        jq -n --arg s "$1" --arg t "$(date -Iseconds)" '{state: $s, at: $t, warnings: $ARGS.positional}' --args "${warnings[@]}" > "$MARK_DIR/claude-plugins.json.tmp" && mv "$MARK_DIR/claude-plugins.json.tmp" "$MARK_DIR/claude-plugins.json" || echo "warning: could not write $MARK_DIR/claude-plugins.json" >&2
      }
      sync_state running
      settings=$HOME_DIR/.claude/settings.json
      known=$HOME_DIR/.claude/plugins/known_marketplaces.json
      plugins=$(jq -r '.enabledPlugins // {} | to_entries[] | select(.value == true) | .key' "$settings") || { warn "cannot read enabledPlugins from $settings"; plugins=; }
      for m in $(printf '%s\n' "$plugins" | sed -n 's/.*@//p' | sort -u); do
        # add only marketplaces missing on disk: 'marketplace add' also writes the marketplace into settings.json, which would dirty the config repo checkout on every start
        if [ -f "$known" ] && jq -e --arg m "$m" 'has($m)' "$known" >/dev/null; then continue; fi
        src=$(jq -r --arg m "$m" '.extraKnownMarketplaces[$m].source | .repo // .url // .path // empty' "$settings") || src=
        if [ -z "$src" ] && [ "$m" = claude-plugins-official ]; then src=anthropics/claude-plugins-official; fi
        if [ -z "$src" ]; then warn "plugin marketplace $m has no source in extraKnownMarketplaces"; continue; fi
        timeout 120 claude plugin marketplace add "$src" </dev/null || warn "could not add plugin marketplace $m"
      done
      timeout 300 claude plugin marketplace update </dev/null || warn "could not update the plugin marketplaces"
      for p in $plugins; do
        { timeout 120 claude plugin install "$p" && timeout 120 claude plugin update "$p"; } </dev/null || warn "could not install or update plugin $p"
      done
      sync_state "done"
    ) &
  fi
fi

if [ ! -d "$WORKSPACE/.git" ]; then
  # clone beside the workspace and move it in only when complete: a container stopped mid-clone would otherwise leave a half .git that this check takes for a done clone forever
  rm -rf "$WORKSPACE.clone"
  git clone "$url" "$WORKSPACE.clone"
  if [ -d "$WORKSPACE" ]; then rmdir "$WORKSPACE"; fi
  mv "$WORKSPACE.clone" "$WORKSPACE"
fi

cd "$WORKSPACE"
# a bad version pin or a download failure only warns: the tunnel must start so the file can be fixed from inside
mise install || echo "warning: mise install failed, fix .mise.toml and restart (or run mise install in the container)" >&2

mkdir -p "$MARK_DIR"
if [ -f .devcontainer/setup.sh ] && { [ ! -f "$MARK_DIR/setup.done" ] || [ "${SETUP_RERUN:-0}" = "1" ]; }; then
  if bash .devcontainer/setup.sh; then
    touch "$MARK_DIR/setup.done"
  else
    echo "warning: .devcontainer/setup.sh failed, it runs again on the next start" >&2
  fi
fi

# GET /status on :8080 for the dashboard; git is read in-process and the tunnel and mise answers are cached, so frequent polls start almost no programs
/usr/local/bin/dockbench-agent &

SERVER_DATA=$HOME_DIR/.vscode-server
MACHINE=$SERVER_DATA/data/Machine
IMAGE_SETTINGS=/opt/dockbench/vscode/settings.json
# seed once: edits made in the container's "Remote [Tunnel]" settings tab survive restarts. A file still identical to the image's counts as not seeded yet, so a VSCODE_SETTINGS (base64, from the dashboard) that was empty or invalid at the first start still applies on a later redeploy. Every seed failure only warns: it must never stop the tunnel.
if [ "${VSCODE_CONFIG:-1}" = "1" ] && { [ ! -f "$MACHINE/settings.json" ] || cmp -s "$MACHINE/settings.json" "$IMAGE_SETTINGS"; }; then
  mkdir -p "$MACHINE" || echo "warning: cannot create $MACHINE" >&2
  # decode beside the target and move it in only on success, so a bad value never leaves a broken file; it falls back to the image's file below
  if [ -n "${VSCODE_SETTINGS:-}" ] && ! { printf '%s' "$VSCODE_SETTINGS" | base64 -d > "$MACHINE/settings.json.tmp" && mv "$MACHINE/settings.json.tmp" "$MACHINE/settings.json"; }; then
    rm -f "$MACHINE/settings.json.tmp" || true
    echo "warning: VSCODE_SETTINGS could not be seeded (invalid base64?), using the image's VS Code settings until a valid value is deployed" >&2
  fi
  if [ ! -f "$MACHINE/settings.json" ]; then
    cp "$IMAGE_SETTINGS" "$MACHINE/settings.json" || echo "warning: VS Code settings not seeded" >&2
  fi
fi

ext=()
ids=()
# split on blanks and newlines (several ids per line, CRLF and a missing trailing newline tolerated) without glob expansion; read -d '' returns 1 at end of input
IFS=$' \t\r\n' read -rd '' -a ids < <(sed 's/#.*//' /opt/dockbench/vscode/extensions.txt) || true
for id in "${ids[@]}"; do ext+=(--install-extension "$id"); done
if [ "${INSTALL_CLAUDE:-1}" = "1" ]; then ext+=(--install-extension anthropic.claude-code); fi

exec code tunnel --accept-server-license-terms --name "$TUNNEL_NAME" --server-data-dir "$SERVER_DATA" "${ext[@]}"
