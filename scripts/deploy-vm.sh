#!/usr/bin/env bash
set -euo pipefail

vm_host="${VM_HOST:-arch@192.168.122.9}"
provider="${PROVIDER:-glm}"
listen="${LISTEN:-127.0.0.1:15722}"
binary_name="codex-mcp-spawner"
local_binary="target/release/${binary_name}"
remote_binary="${REMOTE_BINARY:-/home/arch/.local/bin/${binary_name}}"
remote_staged="${REMOTE_STAGED:-/tmp/${binary_name}.new}"
proxy_log="${PROXY_LOG:-/tmp/codex-glm-proxy.log}"
run_smoke="${RUN_SMOKE:-0}"
skill_source="${SKILL_SOURCE:-skills/n/SKILL.md}"
remote_skill_dir="${REMOTE_SKILL_DIR:-/home/arch/.codex/skills/n}"

log() {
  printf '[deploy] %s\n' "$*"
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf '[deploy] missing required command: %s\n' "$1" >&2
    exit 1
  fi
}

build_release() {
  require_cmd cargo
  log "building release binary"
  cargo build --release

  if [[ ! -x "${local_binary}" ]]; then
    printf '[deploy] build did not produce executable: %s\n' "${local_binary}" >&2
    exit 1
  fi
}

upload_binary() {
  require_cmd scp
  log "uploading ${local_binary} to ${vm_host}:${remote_staged}"
  scp "${local_binary}" "${vm_host}:${remote_staged}"
}

sync_skill() {
  require_cmd scp
  require_cmd ssh
  if [[ ! -f "${skill_source}" ]]; then
    printf '[deploy] missing skill source: %s\n' "${skill_source}" >&2
    exit 1
  fi

  log "syncing n skill to ${vm_host}:${remote_skill_dir}/SKILL.md"
  ssh "${vm_host}" "set -euo pipefail; mkdir -p '${remote_skill_dir}'"
  scp "${skill_source}" "${vm_host}:${remote_skill_dir}/SKILL.md"
}

install_and_restart_proxy() {
  require_cmd ssh
  log "installing binary and restarting ${provider} proxy on ${vm_host}"
  ssh "${vm_host}" \
    "set -euo pipefail
     install -m 755 '${remote_staged}' '${remote_binary}'

     proxy_pids=\$(pgrep -f '^${remote_binary} proxy' || true)
     if [ -n \"\${proxy_pids}\" ]; then
       kill \${proxy_pids}
       sleep 1
     fi

     server_pids=\$(pgrep -f '^${remote_binary}$' || true)
     if [ -n \"\${server_pids}\" ]; then
       kill \${server_pids}
     fi

     nohup '${remote_binary}' proxy --provider '${provider}' --listen '${listen}' > '${proxy_log}' 2>&1 &
     sleep 1

     pgrep -af '^${remote_binary} proxy --provider ${provider} --listen ${listen}'
     tail -n 5 '${proxy_log}'"
}

smoke_test_subagent_write() {
  if [[ "${run_smoke}" != "1" ]]; then
    log "skipping subagent write smoke test; set RUN_SMOKE=1 to enable"
    return
  fi

  log "running subagent write smoke test"
  ssh "${vm_host}" \
    "set -euo pipefail
     probe='/home/arch/codex_subagent_deploy_probe.txt'
     output='/tmp/codex-subagent-deploy-probe-out.txt'
     rm -f \"\${probe}\" \"\${output}\"
     CODEX_HOME=/home/arch/.codex-subagent-debug codex exec --skip-git-repo-check --json --sandbox workspace-write -C /home/arch -m glm-5.2 -o \"\${output}\" 'Create /home/arch/codex_subagent_deploy_probe.txt containing exactly ok using a shell command, then report whether it exists.'
     test \"\$(cat \"\${probe}\")\" = ok
     printf '[deploy] smoke output: '
     cat \"\${output}\"
     printf '\n[deploy] smoke file: '
     ls -l \"\${probe}\""
}

main() {
  build_release
  upload_binary
  sync_skill
  install_and_restart_proxy
  smoke_test_subagent_write
  log "done"
}

main "$@"
