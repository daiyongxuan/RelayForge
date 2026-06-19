#!/usr/bin/env bash
# RelayForge — one-click deploy: Codex + DeepSeek 1M + GLM subagent
set -euo pipefail

# ── Defaults (overridable via secrets.env or env vars) ────────────────
: "${CODEX_HOME:="${HOME}/.codex"}"
: "${SPAWNER_HOME:="${HOME}/.local/bin"}"
: "${CC_SWITCH_VERSION:="v3.16.2"}"
: "${CC_SWITCH_PORT:="15721"}"
: "${GLM_PROXY_PORT:="15722"}"
: "${DEEPSEEK_MODEL:="deepseek-v4-pro"}"
: "${GLM_MODEL:="glm-5.2"}"
: "${CONTEXT_WINDOW:="1000000"}"
: "${COMPACT_LIMIT:="600000"}"
: "${SPAWNER_REPO:="https://github.com/farion1231/codex-mcp-spawner.git"}"
: "${SPAWNER_REPO_DIR:="${HOME}/.local/src/codex-mcp-spawner"}"
: "${SKIP_CODEX_INSTALL:="0"}"
: "${SKIP_CCSWITCH:="0"}"
: "${SKIP_SPAWNER_BUILD:="0"}"
: "${SKIP_VERIFY:="0"}"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'

log()  { printf "${CYAN}[relayforge]${NC} %s\n" "$*"; }
ok()   { printf "${GREEN}[  OK  ]${NC} %s\n" "$*"; }
warn() { printf "${YELLOW}[ WARN ]${NC} %s\n" "$*" >&2; }
err()  { printf "${RED}[ FAIL ]${NC} %s\n" "$*" >&2; }
die()  { err "$*"; exit 1; }

# ── Usage ─────────────────────────────────────────────────────────────
usage() {
  cat <<USAGE
Usage: $0 [OPTIONS]

One-click deploy RelayForge: Codex CLI + DeepSeek 1M context + GLM subagent.

Options:
  --skip-codex          Skip Codex CLI installation
  --skip-ccswitch       Skip cc-switch setup
  --skip-spawner-build  Skip building codex-mcp-spawner from source
  --skip-verify         Skip post-deploy verification
  --help, -h            Show this help

Prerequisites:
  - \$HOME/.config/relayforge/secrets.env with DEEPSEEK_API_KEY and GLM_API_KEY
  - Supported OS: Arch Linux, Debian, Ubuntu
  - Internet access for downloading dependencies

Files created:
  ~/.config/relayforge/secrets.env  (you create this before running)
  ~/.codex/config.toml              Codex provider + MCP config
  ~/.codex/model-catalog.json       DeepSeek 1M model metadata
  ~/.codex/mcp-spawner.toml         GLM subagent provider config
  ~/.codex/skills/n/      Subagent delegation skill
  ~/.cc-switch/cc-switch.db         cc-switch provider + proxy DB
  ~/.local/bin/codex-mcp-spawner    Subagent MCP server binary
  ~/.config/systemd/user/           cc-switch + GLM proxy services
USAGE
  exit 0
}

# ── Argument parsing ───────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-codex) SKIP_CODEX_INSTALL=1 ;;
    --skip-ccswitch) SKIP_CCSWITCH=1 ;;
    --skip-spawner-build) SKIP_SPAWNER_BUILD=1 ;;
    --skip-verify) SKIP_VERIFY=1 ;;
    --help|-h) usage ;;
    *) die "unknown argument: $1" ;;
  esac
  shift
done

# ── OS detection ───────────────────────────────────────────────────────
detect_os() {
  if [ -f /etc/os-release ]; then
    . /etc/os-release
    OS_ID="${ID}"
    OS_LIKE="${ID_LIKE:-}"
  elif [ -f /etc/arch-release ]; then
    OS_ID="arch"
  else
    die "cannot detect OS; supported: Arch Linux, Debian, Ubuntu"
  fi
  log "detected OS: ${OS_ID}"
}

# ── Secrets ────────────────────────────────────────────────────────────
load_secrets() {
  local secrets_file="${HOME}/.config/relayforge/secrets.env"
  if [ ! -f "${secrets_file}" ]; then
    die "missing ${secrets_file}. Copy from scripts/secrets.env.example and fill in your keys."
  fi
  set -a
  source "${secrets_file}"
  set +a
  if [ -z "${DEEPSEEK_API_KEY:-}" ] || [ "${DEEPSEEK_API_KEY}" = "sk-YOUR_DEEPSEEK_API_KEY_HERE" ]; then
    die "DEEPSEEK_API_KEY is not set in ${secrets_file}"
  fi
  if [ -z "${GLM_API_KEY:-}" ] || [ "${GLM_API_KEY}" = "YOUR_GLM_API_KEY_HERE" ]; then
    die "GLM_API_KEY is not set in ${secrets_file}"
  fi
  ok "secrets loaded from ${secrets_file}"
}

# ── Dotfile management ─────────────────────────────────────────────────
write_if_diff() {
  local path="$1" content="$2" desc="$3"
  mkdir -p "$(dirname "${path}")"
  if [ -f "${path}" ]; then
    local current
    current="$(sha256sum "${path}" | cut -d' ' -f1)"
    local wanted
    wanted="$(echo -n "${content}" | sha256sum | cut -d' ' -f1)"
    if [ "${current}" = "${wanted}" ]; then
      ok "${desc} (unchanged)"
      return
    fi
    # Backup existing before overwriting
    cp "${path}" "${path}.bak.$(date +%s)"
    warn "${desc} changed; old version backed up"
  fi
  echo -n "${content}" > "${path}"
  ok "${desc} written"
}

# ── Step 1: Install system deps ────────────────────────────────────────
install_system_deps() {
  log "installing system dependencies"

  case "${OS_ID}" in
    arch|archarm|manjaro)
      local missing=""
      command -v fuse2 >/dev/null 2>&1 || command -v fusermount >/dev/null 2>&1 || missing+=" fuse2"
      command -v xvfb-run >/dev/null 2>&1 || missing+=" xorg-server-xvfb"
      command -v sqlite3 >/dev/null 2>&1 || missing+=" sqlite3"
      # webkit2gtk-4.1 check via pkg-config
      pkg-config --exists webkit2gtk-4.1 2>/dev/null || missing+=" webkit2gtk-4.1"
      if [ -n "${missing}" ]; then
        log "installing:${missing}"
        sudo pacman -S --noconfirm ${missing}
      fi
      ;;
    debian|ubuntu|linuxmint|pop)
      local missing=""
      command -v xvfb-run >/dev/null 2>&1 || missing+=" xvfb"
      command -v sqlite3 >/dev/null 2>&1 || missing+=" sqlite3"
      dpkg -s fuse3 2>/dev/null | grep -q 'ok installed' || missing+=" fuse3"
      pkg-config --exists webkit2gtk-4.1 2>/dev/null || missing+=" libwebkit2gtk-4.1-dev"
      if [ -n "${missing}" ]; then
        sudo apt-get update -qq
        sudo apt-get install -y -qq ${missing}
      fi
      ;;
    *) die "unsupported OS: ${OS_ID}" ;;
  esac
  ok "system dependencies ready"
}

# ── Step 2: Install Codex CLI ──────────────────────────────────────────
install_codex() {
  if [ "${SKIP_CODEX_INSTALL}" = "1" ]; then
    log "skipping Codex CLI install (--skip-codex)"
    return
  fi
  if command -v codex >/dev/null 2>&1; then
    local ver
    ver=$(codex --version 2>/dev/null || echo "unknown")
    ok "codex CLI already installed: ${ver}"
    return
  fi
  log "installing Codex CLI via npm"
  if ! command -v npm >/dev/null 2>&1; then
    case "${OS_ID}" in
      arch|archarm|manjaro) sudo pacman -S --noconfirm npm ;;
      debian|ubuntu|linuxmint|pop) sudo apt-get install -y -qq npm ;;
    esac
  fi
  # Install globally if we have sudo, otherwise user-local
  if sudo -n true 2>/dev/null; then
    sudo npm install -g "@openai/codex@latest"
  else
    npm install -g "@openai/codex@latest"
    # Ensure npm global bin is on PATH
    export PATH="$(npm config get prefix)/bin:${PATH}"
  fi
  command -v codex >/dev/null 2>&1 || die "codex CLI not found after install. Check PATH."
  ok "codex CLI installed: $(codex --version)"
}

# ── Step 3: cc-switch AppImage ─────────────────────────────────────────
setup_ccswitch() {
  if [ "${SKIP_CCSWITCH}" = "1" ]; then
    log "skipping cc-switch setup (--skip-ccswitch)"
    return
  fi

  local cc_bin="${HOME}/cc-switch.AppImage"
  local cc_url="https://github.com/farion1231/cc-switch/releases/download/${CC_SWITCH_VERSION}/CC-Switch-${CC_SWITCH_VERSION}-Linux-x86_64.AppImage"

  # Download if missing
  if [ ! -x "${cc_bin}" ]; then
    log "downloading cc-switch ${CC_SWITCH_VERSION}"
    curl -fL -o "${cc_bin}" "${cc_url}" || die "failed to download cc-switch"
    chmod +x "${cc_bin}"
  fi
  ok "cc-switch AppImage: ${cc_bin} ($(du -h "${cc_bin}" | cut -f1))"

  # Initialize DB on first run
  local cc_db="${HOME}/.cc-switch/cc-switch.db"
  local first_launch="0"
  if [ ! -f "${cc_db}" ]; then
    first_launch="1"
    log "first launch: initializing cc-switch database"
    export GALLIUM_DRIVER=llvmpipe LIBGL_ALWAYS_SOFTWARE=1
    export WEBKIT_DISABLE_COMPOSITING_MODE=1 WEBKIT_DISABLE_DMABUF_RENDERER=1
    nohup xvfb-run -a "${cc_bin}" > /tmp/ccswitch-init.log 2>&1 &
    local cc_pid=$!
    # Wait up to 15s for DB to appear
    for i in $(seq 1 15); do
      sleep 1
      if [ -f "${cc_db}" ] && [ -s "${cc_db}" ]; then break; fi
    done
    kill ${cc_pid} 2>/dev/null || true
    sleep 1
    pkill -f "cc-switch" 2>/dev/null || true
    [ -f "${cc_db}" ] || die "cc-switch database not created. Check /tmp/ccswitch-init.log"
    ok "cc-switch database initialized"
  fi

  # Enable proxy for Codex
  sqlite3 "${cc_db}" \
    "UPDATE proxy_config SET proxy_enabled=1, enabled=1 WHERE app_type='codex';"

  # Write DeepSeek provider config
  python3 << PYEOF
import json, sqlite3

API_KEY  = "${DEEPSEEK_API_KEY}"
API_HOST = "https://api.deepseek.com"
PORT     = "${CC_SWITCH_PORT}"
MODEL    = "${DEEPSEEK_MODEL}"
CTX      = ${CONTEXT_WINDOW}

db = sqlite3.connect("${HOME}/.cc-switch/cc-switch.db")

settings_config = json.dumps({
    "auth": {},
    "base_url": API_HOST,
    "api_key": API_KEY,
    "api_format": "openai_chat",
    "config": (
        'model_provider = "custom"\n'
        f'model = "{MODEL}"\n'
        '\n'
        '[model_providers.custom]\n'
        'name = "DeepSeek"\n'
        f'base_url = "http://127.0.0.1:{PORT}/v1"\n'
        'env_key = "DEEPSEEK_API_KEY"\n'
        'wire_api = "responses"\n'
    ),
}, ensure_ascii=False)

meta = json.dumps({
    "api_format": "openai_chat",
    "context_window": CTX
}, ensure_ascii=False)

db.execute(
    "UPDATE providers SET settings_config=?, meta=? "
    "WHERE id='default' AND app_type='codex'",
    (settings_config, meta)
)
db.commit()
db.close()
PYEOF
  ok "cc-switch provider configured (DeepSeek, api_format=openai_chat)"
}

# ── Step 4: Write Codex config.toml ────────────────────────────────────
write_codex_config() {
  local mcp_config=""
  mcp_config=$(cat <<MCP
[mcp_servers.subagent-spawner]
command = "${SPAWNER_HOME}/codex-mcp-spawner"
startup_timeout_sec = 10
tool_timeout_sec = 900

[mcp_servers.subagent-spawner.tools.spawn_agent]
approval_mode = "approve"
MCP
)

  local config
  config=$(cat <<CONFIG
model_provider = "deepseek"
model = "${DEEPSEEK_MODEL}"
model_catalog_json = "${CODEX_HOME}/model-catalog.json"
model_context_window = ${CONTEXT_WINDOW}
model_auto_compact_token_limit = ${COMPACT_LIMIT}

[model_providers.deepseek]
name = "DeepSeek"
base_url = "http://127.0.0.1:${CC_SWITCH_PORT}/v1"
wire_api = "responses"
requires_openai_auth = false

${mcp_config}
CONFIG
)
  write_if_diff "${CODEX_HOME}/config.toml" "${config}" "Codex config.toml"
}

# ── Step 5: Write model catalog ────────────────────────────────────────
write_model_catalog() {
  local catalog
  catalog=$(python3 << PYEOF
import json
def model(slug, display_name, description, priority):
  return {
    "slug": slug,
    "display_name": display_name,
    "description": description,
    "context_window": ${CONTEXT_WINDOW},
    "max_context_window": ${CONTEXT_WINDOW},
    "auto_compact_token_limit": ${COMPACT_LIMIT},
    "default_reasoning_level": "medium",
    "supported_reasoning_levels": [
      {"effort": "low", "description": "Fast"},
      {"effort": "medium", "description": "Balanced"},
      {"effort": "high", "description": "Deep reasoning"},
      {"effort": "xhigh", "description": "Extra high reasoning"}
    ],
    "shell_type": "shell_command",
    "visibility": "list",
    "supported_in_api": True,
    "priority": priority,
    "additional_speed_tiers": [],
    "service_tiers": [],
    "base_instructions": "",
    "supports_reasoning_summaries": False,
    "default_reasoning_summary": "none",
    "support_verbosity": False,
    "default_verbosity": None,
    "apply_patch_tool_type": "freeform",
    "web_search_tool_type": "text_and_image",
    "truncation_policy": {"mode": "tokens", "limit": 10000},
    "supports_parallel_tool_calls": True,
    "supports_image_detail_original": False,
    "effective_context_window_percent": 95,
    "experimental_supported_tools": [],
    "input_modalities": ["text"],
    "supports_search_tool": False,
    "use_responses_lite": False
  }

catalog = {
  "models": [
    model("${DEEPSEEK_MODEL}", "${DEEPSEEK_MODEL}", "${DEEPSEEK_MODEL} via cc-switch (RelayForge)", 0),
    model("${GLM_MODEL}", "${GLM_MODEL}", "${GLM_MODEL} via RelayForge local proxy", 1),
  ]
}
print(json.dumps(catalog, indent=2))
PYEOF
)
  write_if_diff "${CODEX_HOME}/model-catalog.json" "${catalog}" "model catalog (DeepSeek + GLM 1M)"
}

# ── Step 6: Write mcp-spawner.toml (GLM provider) ──────────────────────
write_spawner_config() {
  local config
  config=$(cat <<CONFIG
[providers.glm]
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
proxy_url = "http://127.0.0.1:${GLM_PROXY_PORT}"
api_key = "${GLM_API_KEY}"
wire_api = "responses"
default_timeout_sec = 1800
models = [
  { slug = "${GLM_MODEL}", context_window = 1000000 },
]
CONFIG
)
  write_if_diff "${CODEX_HOME}/mcp-spawner.toml" "${config}" "mcp-spawner.toml (GLM provider)"
}

# ── Step 7: Build and install codex-mcp-spawner ────────────────────────
install_spawner() {
  if [ "${SKIP_SPAWNER_BUILD}" = "1" ]; then
    log "skipping spawner build (--skip-spawner-build)"
    return
  fi
  if [ -x "${SPAWNER_HOME}/codex-mcp-spawner" ]; then
    ok "codex-mcp-spawner already installed"
    return
  fi

  # Ensure cargo is available
  if ! command -v cargo >/dev/null 2>&1; then
    log "installing Rust toolchain"
    if [ -f "${HOME}/.cargo/env" ]; then
      source "${HOME}/.cargo/env"
    elif ! command -v rustup >/dev/null 2>&1; then
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
      source "${HOME}/.cargo/env"
    fi
  fi

  # Clone or update the repo
  mkdir -p "$(dirname "${SPAWNER_REPO_DIR}")"
  if [ ! -d "${SPAWNER_REPO_DIR}/.git" ]; then
    log "cloning codex-mcp-spawner"
    git clone "${SPAWNER_REPO}" "${SPAWNER_REPO_DIR}"
  else
    log "updating codex-mcp-spawner"
    (cd "${SPAWNER_REPO_DIR}" && git pull --ff-only)
  fi

  log "building codex-mcp-spawner (release)"
  (cd "${SPAWNER_REPO_DIR}" && cargo build --release)
  mkdir -p "${SPAWNER_HOME}"
  install -m 755 "${SPAWNER_REPO_DIR}/target/release/codex-mcp-spawner" "${SPAWNER_HOME}/"
  ok "codex-mcp-spawner installed to ${SPAWNER_HOME}/"
}

# ── Step 8: Install n skill ──────────────────────────────────
install_skill() {
  local skill_dir="${CODEX_HOME}/skills/n"
  mkdir -p "${skill_dir}"
  local skill_file="${skill_dir}/SKILL.md"
  if [ -f "${SPAWNER_REPO_DIR}/skills/n/SKILL.md" ]; then
    cp "${SPAWNER_REPO_DIR}/skills/n/SKILL.md" "${skill_file}"
    ok "n skill installed"
  else
    warn "n SKILL.md not found in repo; writing inline"
    cat > "${skill_file}" << 'SKILLEOF'
---
name: n
description: Delegate isolated implementation work to a GLM subagent. Use for code writing, test generation, refactoring, or batch edits that can be specified without shared conversation context.
---

# When to Use

Use `spawn_agent` with `provider: "glm"` when the task satisfies **all three**:

1. **Self-contained** — the prompt can describe the full job without referencing this conversation.
2. **Code-heavy** — the work is writing/editing code, tests, configs, or docs.
3. **Verifiable** — a clear command exists to check correctness (`cargo test`, `pytest`, `bash -n`, etc.).

Do **not** use for: reading or analyzing code, making decisions, tasks < 10 lines, or anything needing conversation context.

# How to Call

Always include these 6 sections in `message`:

1. CONTEXT: repo path, why this task exists, relevant files to READ first.
2. TASK: exactly what to implement/change. Be specific about behavior.
3. DO NOT: explicit boundaries — files/conventions not to touch.
4. CONSTRAINTS: lint rules, test frameworks, code style from AGENTS.md.
5. VERIFY: exact commands to run for validation.
6. ON FAILURE: "Report the exact error. Do not guess. Do not work around it silently."

Set `cwd` to the repo root. Omit `timeout_sec` to use the provider default; set it explicitly for unusually long builds.

# After Subagent Returns

1. Read `final_message` — if it reports an error, trust it. Do not re-run the same prompt.
2. If `exit_code != 0` or `final_message` is empty: the subagent failed. Either fix it yourself or spawn a corrected prompt.
3. If `exit_code == 0`: open the changed files and verify the diff makes sense. Remove dead code or overlap.
4. Run the verification command yourself before reporting success to the user.
5. Report to the user: what changed, whether tests pass, any caveats.

# Retry Pattern

If subagent fails, spawn again with a **corrected prompt** — add the error message, narrow the scope, or clarify instructions. Never retry the identical prompt expecting different results.

SKILLEOF
  fi
}

# ── Step 9: systemd user services ──────────────────────────────────────
setup_systemd_user() {
  if ! command -v systemctl >/dev/null 2>&1; then
    warn "systemctl not found; skipping systemd user service setup"
    return
  fi

  local unit_dir="${HOME}/.config/systemd/user"
  mkdir -p "${unit_dir}"

  cat > "${unit_dir}/cc-switch.service" << UNITEOF
[Unit]
Description=cc-switch proxy (DeepSeek Responses ↔ Chat)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=xvfb-run -a ${HOME}/cc-switch.AppImage
Environment="GALLIUM_DRIVER=llvmpipe"
Environment="LIBGL_ALWAYS_SOFTWARE=1"
Environment="WEBKIT_DISABLE_COMPOSITING_MODE=1"
Environment="WEBKIT_DISABLE_DMABUF_RENDERER=1"
Environment="DEEPSEEK_API_KEY=${DEEPSEEK_API_KEY}"
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
UNITEOF

  cat > "${unit_dir}/relayforge-glm-proxy.service" << UNITEOF
[Unit]
Description=RelayForge GLM provider proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${SPAWNER_HOME}/codex-mcp-spawner proxy --provider glm --listen 127.0.0.1:${GLM_PROXY_PORT}
Environment="CODEX_HOME=${CODEX_HOME}"
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
UNITEOF

  systemctl --user daemon-reload
  systemctl --user enable cc-switch.service relayforge-glm-proxy.service 2>/dev/null || true
  ok "systemd user services configured"
}
start_services() {
  log "starting services"

  # Kill any existing instances to pick up new config
  pkill -f "cc-switch" 2>/dev/null || true
  pkill -f "codex-mcp-spawner proxy" 2>/dev/null || true
  sleep 2

  if command -v systemctl >/dev/null 2>&1; then
    systemctl --user restart cc-switch.service relayforge-glm-proxy.service 2>/dev/null || true
  else
    # Fallback: nohup
    export GALLIUM_DRIVER=llvmpipe LIBGL_ALWAYS_SOFTWARE=1
    export WEBKIT_DISABLE_COMPOSITING_MODE=1 WEBKIT_DISABLE_DMABUF_RENDERER=1
    nohup xvfb-run -a "${HOME}/cc-switch.AppImage" > /tmp/ccswitch.log 2>&1 &
    nohup "${SPAWNER_HOME}/codex-mcp-spawner" proxy --provider glm --listen "127.0.0.1:${GLM_PROXY_PORT}" > /tmp/relayforge-glm-proxy.log 2>&1 &
  fi

  # Wait for cc-switch port
  log "waiting for cc-switch on :${CC_SWITCH_PORT}..."
  for i in $(seq 1 15); do
    if ss -tlnp 2>/dev/null | grep -q ":${CC_SWITCH_PORT} " || netstat -tlnp 2>/dev/null | grep -q ":${CC_SWITCH_PORT} "; then
      break
    fi
    sleep 1
  done
  if ss -tlnp 2>/dev/null | grep -q ":${CC_SWITCH_PORT} " || netstat -tlnp 2>/dev/null | grep -q ":${CC_SWITCH_PORT} "; then
    ok "cc-switch listening on :${CC_SWITCH_PORT}"
  else
    warn "cc-switch may not be running. Check /tmp/ccswitch.log"
  fi

  # Wait for GLM proxy port
  log "waiting for GLM proxy on :${GLM_PROXY_PORT}..."
  for i in $(seq 1 10); do
    if ss -tlnp 2>/dev/null | grep -q ":${GLM_PROXY_PORT} " || netstat -tlnp 2>/dev/null | grep -q ":${GLM_PROXY_PORT} "; then
      break
    fi
    sleep 1
  done
  if ss -tlnp 2>/dev/null | grep -q ":${GLM_PROXY_PORT} " || netstat -tlnp 2>/dev/null | grep -q ":${GLM_PROXY_PORT} "; then
    ok "GLM proxy listening on :${GLM_PROXY_PORT}"
  else
    warn "GLM proxy may not be running. Check /tmp/relayforge-glm-proxy.log"
  fi
}

# ── Step 11: Verification ──────────────────────────────────────────────
run_verify() {
  if [ "${SKIP_VERIFY}" = "1" ]; then
    log "skipping verification (--skip-verify)"
    return
  fi

  local failed=0
  log "running end-to-end verification"

  # Verify cc-switch model catalog
  echo -n "  model catalog (1M context) ... "
  if curl -s --max-time 10 "http://127.0.0.1:${CC_SWITCH_PORT}/v1/models" | \
    python3 -c "
import sys, json
m = json.load(sys.stdin)['models'][0]
assert m['context_window'] == ${CONTEXT_WINDOW}, f'ctx={m[\"context_window\"]}'
assert m['max_context_window'] == ${CONTEXT_WINDOW}
" 2>/dev/null; then
    echo -e "${GREEN}PASS${NC}"
  else
    echo -e "${RED}FAIL${NC}"
    failed=1
  fi

  # Verify DeepSeek non-streaming
  echo -n "  DeepSeek non-streaming ... "
  local status
  status=$(curl -s --max-time 30 "http://127.0.0.1:${CC_SWITCH_PORT}/v1/responses" \
    -H "Content-Type: application/json" \
    -d "{\"model\":\"${DEEPSEEK_MODEL}\",\"input\":[{\"role\":\"user\",\"content\":\"say ok\"}],\"stream\":false}" \
    | python3 -c "import sys,json; print(json.load(sys.stdin)['status'])" 2>/dev/null)
  if [ "${status}" = "completed" ]; then
    echo -e "${GREEN}PASS${NC}"
  else
    echo -e "${RED}FAIL (status=${status})${NC}"
    failed=1
  fi

  # Verify DeepSeek streaming
  echo -n "  DeepSeek streaming ... "
  local count
  count=$(curl -s -N --max-time 30 "http://127.0.0.1:${CC_SWITCH_PORT}/v1/responses" \
    -H "Content-Type: application/json" \
    -d "{\"model\":\"${DEEPSEEK_MODEL}\",\"input\":[{\"role\":\"user\",\"content\":\"say ok\"}],\"stream\":true}" \
    | grep -c "response.completed" 2>/dev/null || echo 0)
  if [ "${count}" -gt 0 ]; then
    echo -e "${GREEN}PASS (events=${count})${NC}"
  else
    echo -e "${RED}FAIL${NC}"
    failed=1
  fi

  # Verify GLM proxy
  echo -n "  GLM proxy (non-streaming) ... "
  local glm_status
  glm_status=$(curl -s --max-time 30 "http://127.0.0.1:${GLM_PROXY_PORT}/v1/responses" \
    -H "Content-Type: application/json" \
    -d "{\"model\":\"${GLM_MODEL}\",\"input\":[{\"role\":\"user\",\"content\":\"say ok\"}],\"stream\":false}" \
    | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','error'))" 2>/dev/null)
  if [ "${glm_status}" = "completed" ]; then
    echo -e "${GREEN}PASS${NC}"
  else
    echo -e "${RED}FAIL (status=${glm_status})${NC}"
    failed=1
  fi

  if [ "${failed}" -eq 0 ]; then
    ok "all verification checks passed"
  else
    warn "${failed} verification check(s) failed — check logs in /tmp/"
  fi
}

# ── Summary ────────────────────────────────────────────────────────────
print_summary() {
  echo ""
  echo "  ┌──────────────────────────────────────────────────────┐"
  echo "  │              RelayForge Deploy Complete               │"
  echo "  ├──────────────────────────────────────────────────────┤"
  echo "  │ DeepSeek proxy   127.0.0.1:${CC_SWITCH_PORT}                     │"
  echo "  │ GLM proxy        127.0.0.1:${GLM_PROXY_PORT}                     │"
  echo "  │ Codex home       ${CODEX_HOME}                            │"
  echo "  │ Spawner binary   ${SPAWNER_HOME}/codex-mcp-spawner             │"
  echo "  └──────────────────────────────────────────────────────┘"
  echo ""
  log "To start using: run 'codex' in your project directory."
  log "To delegate to GLM subagent: use the spawn_agent MCP tool with provider=\"glm\"."
}

# ── Main ───────────────────────────────────────────────────────────────
main() {
  echo ""
  log "RelayForge one-click deploy starting..."
  echo ""

  detect_os
  load_secrets
  install_system_deps
  install_codex
  setup_ccswitch
  write_codex_config
  write_model_catalog
  write_spawner_config
  install_spawner
  install_skill
  setup_systemd_user
  start_services
  run_verify
  print_summary
}

main
