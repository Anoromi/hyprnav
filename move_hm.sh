#!/usr/bin/env bash
set -euo pipefail

usage() {
  printf '%s\n' \
    "Usage: $0 [--no-flicker] [--match-class <regex>] [--match-initial-class <regex>] [--match-title <regex>] [--match-initial-title <regex>] <workspace> <command...>" >&2
}

quote_shell_arg() {
  printf "'%s'" "${1//\'/\'\"\'\"\'}"
}

join_shell_command() {
  local args=("$@")
  local joined=""
  local arg
  for arg in "${args[@]}"; do
    if [[ -n "$joined" ]]; then
      joined+=" "
    fi
    joined+="$(quote_shell_arg "$arg")"
  done
  printf '%s' "$joined"
}

sanitize_rule_name_segment() {
  local value="${1:-}"
  value="${value//[^a-zA-Z0-9_-]/-}"
  value="${value##-}"
  value="${value%%-}"
  printf '%s' "$value"
}

ensure_environment() {
  if [[ -z "${HYPRLAND_INSTANCE_SIGNATURE:-}" || -z "${XDG_RUNTIME_DIR:-}" ]]; then
    printf '%s\n' "Hyprland does not appear to be available in this environment." >&2
    exit 1
  fi

  if ! command -v hyprctl >/dev/null 2>&1; then
    printf '%s\n' "Missing required command: hyprctl" >&2
    exit 1
  fi

  if ! command -v jq >/dev/null 2>&1; then
    printf '%s\n' "Missing required command: jq" >&2
    exit 1
  fi
}

list_child_pids() {
  local pid="$1"

  if command -v pgrep >/dev/null 2>&1; then
    pgrep -P "$pid" 2>/dev/null || true
    return 0
  fi

  ps -o pid= --ppid "$pid" 2>/dev/null || true
}

collect_process_tree_pids() {
  local pid="$1"

  if [[ -z "$pid" ]]; then
    return 0
  fi

  if ! kill -0 "$pid" >/dev/null 2>&1; then
    return 0
  fi

  printf '%s\n' "$pid"

  local child_pid
  while IFS= read -r child_pid; do
    if [[ -z "$child_pid" ]]; then
      continue
    fi
    collect_process_tree_pids "$child_pid"
  done < <(list_child_pids "$pid")
}

resolve_fallback_identity() {
  if [[ $# -eq 0 || -z "${1:-}" ]]; then
    return 0
  fi

  basename -- "$1" | tr '[:upper:]' '[:lower:]'
}

normalize_identity() {
  local value="${1:-}"
  value="${value,,}"
  printf '%s' "$value"
}

identity_matches_candidate() {
  local identity candidate
  identity="$(normalize_identity "${1:-}")"
  candidate="$(normalize_identity "${2:-}")"

  if [[ -z "$identity" || -z "$candidate" ]]; then
    return 1
  fi

  if [[ "$candidate" == "$identity" ]]; then
    return 0
  fi

  if [[ "$candidate" == */"$identity" || "$candidate" == */"$identity/"* ]]; then
    return 0
  fi

  case "$candidate" in
    *".$identity" | *".$identity".* | *".$identity"-* | *".$identity"_* | \
      *"-$identity" | *"-$identity".* | *"-$identity"-* | *"-$identity"_* | \
      *"_$identity" | *"_$identity".* | *"_$identity"-* | *"_$identity"_*)
      return 0
      ;;
  esac

  return 1
}

read_process_basename() {
  local pid="$1"
  local argv0

  if [[ -z "$pid" ]]; then
    return 0
  fi

  argv0="$(ps -p "$pid" -o args= 2>/dev/null | awk 'NR == 1 { print $1 }')"
  if [[ -z "$argv0" ]]; then
    return 0
  fi

  basename -- "$argv0"
}

client_matches_fallback_identity() {
  local pid="$1"
  local client_class="$2"
  local client_initial_class="$3"
  local process_basename=""

  if [[ -z "${fallback_identity:-}" ]]; then
    return 1
  fi

  process_basename="$(read_process_basename "$pid")"

  if identity_matches_candidate "$fallback_identity" "$process_basename"; then
    return 0
  fi

  if identity_matches_candidate "$fallback_identity" "$client_class"; then
    return 0
  fi

  if identity_matches_candidate "$fallback_identity" "$client_initial_class"; then
    return 0
  fi

  return 1
}

list_client_records() {
  hyprctl -j clients 2>/dev/null |
    jq -r '
      sort_by(.workspace.id // -1, .at[1] // 0, .at[0] // 0, .address)[] |
      "\(.address)\t\(.pid)\t\(.workspace.id // -1)\t\(.at[0] // 0)\t\(.at[1] // 0)\t\(.class // "")\t\(.initialClass // "")"
    '
}

remember_known_client() {
  local address="$1"

  if [[ -z "$address" ]]; then
    return 0
  fi

  if grep -Fxq "$address" <<<"$known_client_addresses"; then
    return 0
  fi

  if [[ -n "$known_client_addresses" ]]; then
    known_client_addresses+=$'\n'
  fi
  known_client_addresses+="$address"
}

cleanup_prelaunch_rules() {
  local rule_name
  for rule_name in "${prelaunch_rule_names[@]:-}"; do
    if [[ -z "$rule_name" ]]; then
      continue
    fi
    hyprctl keyword "windowrule[$rule_name]:enable false" >/dev/null 2>&1 || true
  done
}

install_prelaunch_rule() {
  local match_kind="$1"
  local pattern="$2"
  local rule_name="$3"

  if [[ -z "$pattern" ]]; then
    return 0
  fi

  if ! hyprctl keyword "windowrule[$rule_name]:match:$match_kind $pattern" >/dev/null 2>&1; then
    return 1
  fi

  if ! hyprctl keyword "windowrule[$rule_name]:workspace $workspace silent" >/dev/null 2>&1; then
    hyprctl keyword "windowrule[$rule_name]:enable false" >/dev/null 2>&1 || true
    return 1
  fi

  if ! hyprctl keyword "windowrule[$rule_name]:enable true" >/dev/null 2>&1; then
    hyprctl keyword "windowrule[$rule_name]:enable false" >/dev/null 2>&1 || true
    return 1
  fi

  prelaunch_rule_names+=("$rule_name")
  return 0
}

install_prelaunch_rules() {
  prelaunch_rule_names=()

  local rule_index=0
  local pattern
  local rule_name

  for pattern in "${match_class_patterns[@]:-}"; do
    rule_name="move-hm-class-$(sanitize_rule_name_segment "$$-$rule_index")"
    if ! install_prelaunch_rule "class" "$pattern" "$rule_name"; then
      cleanup_prelaunch_rules
      return 1
    fi
    rule_index=$((rule_index + 1))
  done

  for pattern in "${match_initial_class_patterns[@]:-}"; do
    rule_name="move-hm-initial-class-$(sanitize_rule_name_segment "$$-$rule_index")"
    if ! install_prelaunch_rule "initialclass" "$pattern" "$rule_name"; then
      cleanup_prelaunch_rules
      return 1
    fi
    rule_index=$((rule_index + 1))
  done

  for pattern in "${match_title_patterns[@]:-}"; do
    rule_name="move-hm-title-$(sanitize_rule_name_segment "$$-$rule_index")"
    if ! install_prelaunch_rule "title" "$pattern" "$rule_name"; then
      cleanup_prelaunch_rules
      return 1
    fi
    rule_index=$((rule_index + 1))
  done

  for pattern in "${match_initial_title_patterns[@]:-}"; do
    rule_name="move-hm-initial-title-$(sanitize_rule_name_segment "$$-$rule_index")"
    if ! install_prelaunch_rule "initialtitle" "$pattern" "$rule_name"; then
      cleanup_prelaunch_rules
      return 1
    fi
    rule_index=$((rule_index + 1))
  done
}

move_tracked_windows() {
  mapfile -t scope_pids < <(collect_process_tree_pids "$launcher_pid" | sort -u)
  mapfile -t client_records < <(list_client_records)

  local pid_list
  pid_list="$(printf '%s\n' "${scope_pids[@]:-}")"

  local client_record
  for client_record in "${client_records[@]}"; do
    IFS=$'\t' read -r address pid current_workspace _client_x _client_y client_class client_initial_class <<<"$client_record"
    if [[ -z "$address" || -z "$pid" || -z "$current_workspace" ]]; then
      continue
    fi

    client_is_new=0
    client_matches_process_tree=0

    if ! grep -Fxq "$address" <<<"$known_client_addresses"; then
      client_is_new=1
    fi

    if [[ -n "$pid_list" ]] && grep -Fxq "$pid" <<<"$pid_list"; then
      client_matches_process_tree=1
      saw_process_tree_client=1
    fi

    if ((client_matches_process_tree == 0)); then
      if ((allow_new_client_fallback == 0 || client_is_new == 0)); then
        continue
      fi

      if ! client_matches_fallback_identity "$pid" "$client_class" "$client_initial_class"; then
        continue
      fi
    fi

    if ((client_is_new == 0 && client_matches_process_tree == 0)); then
      continue
    fi

    if [[ "$current_workspace" != "$workspace" ]]; then
      hyprctl dispatch movetoworkspacesilent "$workspace,address:$address" >/dev/null 2>&1 || true
    fi

    remember_known_client "$address"
  done
}

cleanup_child() {
  if [[ -n "${launcher_pid:-}" ]] && kill -0 "$launcher_pid" >/dev/null 2>&1; then
    kill -TERM "$launcher_pid" >/dev/null 2>&1 || true
  fi
}

cleanup_control_dir() {
  if [[ -n "${control_dir:-}" && -d "${control_dir:-}" ]]; then
    rm -rf "$control_dir"
  fi
}

run_attached() {
  local poll_interval_s="0.2"
  local post_exit_grace_polls=10
  local post_exit_polls_remaining="$post_exit_grace_polls"
  local exit_code=0
  allow_new_client_fallback=0
  saw_process_tree_client=0
  fallback_identity="$(resolve_fallback_identity "${1:-}")"

  known_client_addresses="$(list_client_records | cut -f1)"

  if ! install_prelaunch_rules; then
    printf '%s\n' "move_hm.sh: failed to install one or more Hyprland prelaunch rules." >&2
    exit 1
  fi

  trap 'cleanup_child; cleanup_prelaunch_rules; exit 130' INT
  trap 'cleanup_child; cleanup_prelaunch_rules; exit 143' TERM
  trap 'cleanup_child; cleanup_prelaunch_rules; exit 129' HUP

  env T3CODE_HYPR_WORKSPACE="$workspace" "$@" &
  launcher_pid=$!

  while true; do
    move_tracked_windows

    if ! kill -0 "$launcher_pid" >/dev/null 2>&1; then
      if ((saw_process_tree_client == 0)); then
        allow_new_client_fallback=1
      fi
      post_exit_polls_remaining=$((post_exit_polls_remaining - 1))
      if ((post_exit_polls_remaining <= 0)); then
        break
      fi
    else
      allow_new_client_fallback=0
    fi

    sleep "$poll_interval_s"
  done

  if ! wait "$launcher_pid"; then
    exit_code=$?
  fi

  cleanup_prelaunch_rules

  return "$exit_code"
}

run_no_flicker() {
  local script_dir script_path launch_command wrapped_command
  local launch_args=()
  script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
  script_path="$script_dir/$(basename -- "$0")"
  control_dir="$(mktemp -d "${XDG_RUNTIME_DIR:-/tmp}/move-hm.XXXXXX")"
  control_log_file="$control_dir/output.log"
  control_worker_pid_file="$control_dir/worker.pid"
  control_exit_code_file="$control_dir/exit.code"
  : >"$control_log_file"

  launch_args=(
    "$script_path"
    "--internal-launch"
    "--control-log-file" "$control_log_file"
    "--control-worker-pid-file" "$control_worker_pid_file"
    "--control-exit-code-file" "$control_exit_code_file"
  )

  local pattern
  for pattern in "${match_class_patterns[@]:-}"; do
    launch_args+=("--match-class" "$pattern")
  done
  for pattern in "${match_initial_class_patterns[@]:-}"; do
    launch_args+=("--match-initial-class" "$pattern")
  done
  for pattern in "${match_title_patterns[@]:-}"; do
    launch_args+=("--match-title" "$pattern")
  done
  for pattern in "${match_initial_title_patterns[@]:-}"; do
    launch_args+=("--match-initial-title" "$pattern")
  done

  launch_args+=("$workspace" "$@")

  launch_command=$(
    join_shell_command "${launch_args[@]}"
  )
  wrapped_command=$(
    join_shell_command \
      bash \
      -lc \
      "cd $(quote_shell_arg "$PWD") && exec setsid -f bash -lc $(quote_shell_arg "exec $launch_command") </dev/null >/dev/null 2>&1"
  )

  trap 'if [[ -s "${control_worker_pid_file:-}" ]]; then kill -TERM -- "-$(cat "$control_worker_pid_file")" >/dev/null 2>&1 || true; fi; cleanup_control_dir; exit 130' INT
  trap 'if [[ -s "${control_worker_pid_file:-}" ]]; then kill -TERM -- "-$(cat "$control_worker_pid_file")" >/dev/null 2>&1 || true; fi; cleanup_control_dir; exit 143' TERM
  trap 'if [[ -s "${control_worker_pid_file:-}" ]]; then kill -TERM -- "-$(cat "$control_worker_pid_file")" >/dev/null 2>&1 || true; fi; cleanup_control_dir; exit 129' HUP

  hyprctl dispatch exec "[workspace $workspace silent] $wrapped_command" >/dev/null

  for _ in $(seq 1 100); do
    if [[ -s "$control_worker_pid_file" ]]; then
      break
    fi
    sleep 0.05
  done

  tail -n +1 -f "$control_log_file" &
  tail_pid=$!

  while [[ ! -f "$control_exit_code_file" ]]; do
    if [[ -s "$control_worker_pid_file" ]] &&
      ! kill -0 "$(cat "$control_worker_pid_file")" >/dev/null 2>&1; then
      printf '%s\n' "move_hm.sh: detached launcher exited before reporting a status." >&2
      break
    fi
    sleep 0.1
  done

  if [[ -f "$control_exit_code_file" ]]; then
    exit_code="$(cat "$control_exit_code_file")"
  else
    exit_code=1
  fi

  kill "$tail_pid" >/dev/null 2>&1 || true
  wait "$tail_pid" 2>/dev/null || true
  cleanup_control_dir
  exit "$exit_code"
}

no_flicker=0
internal_launch=0
control_log_file=""
control_worker_pid_file=""
control_exit_code_file=""
match_class_patterns=()
match_initial_class_patterns=()
match_title_patterns=()
match_initial_title_patterns=()

while (($# > 0)); do
  case "$1" in
    --no-flicker)
      no_flicker=1
      shift
      ;;
    --internal-launch)
      internal_launch=1
      shift
      ;;
    --control-log-file)
      control_log_file="$2"
      shift 2
      ;;
    --control-worker-pid-file)
      control_worker_pid_file="$2"
      shift 2
      ;;
    --control-exit-code-file)
      control_exit_code_file="$2"
      shift 2
      ;;
    --match-class)
      if (($# < 2)); then
        printf '%s\n' "Missing value for '--match-class'." >&2
        exit 1
      fi
      match_class_patterns+=("$2")
      shift 2
      ;;
    --match-initial-class)
      if (($# < 2)); then
        printf '%s\n' "Missing value for '--match-initial-class'." >&2
        exit 1
      fi
      match_initial_class_patterns+=("$2")
      shift 2
      ;;
    --match-title)
      if (($# < 2)); then
        printf '%s\n' "Missing value for '--match-title'." >&2
        exit 1
      fi
      match_title_patterns+=("$2")
      shift 2
      ;;
    --match-initial-title)
      if (($# < 2)); then
        printf '%s\n' "Missing value for '--match-initial-title'." >&2
        exit 1
      fi
      match_initial_title_patterns+=("$2")
      shift 2
      ;;
    --)
      shift
      break
      ;;
    -*)
      printf "Unknown option: %s\n" "$1" >&2
      usage
      exit 1
      ;;
    *)
      break
      ;;
  esac
done

if [ "$#" -lt 2 ]; then
  usage
  exit 1
fi

workspace="$1"
shift

if [[ ! "$workspace" =~ ^[1-9][0-9]*$ ]]; then
  printf '%s\n' "Invalid workspace '$workspace': expected a positive integer." >&2
  exit 1
fi

ensure_environment

if ((internal_launch == 1)); then
  if [[ -n "$control_worker_pid_file" ]]; then
    printf '%s\n' "$$" >"$control_worker_pid_file"
  fi

  if [[ -n "$control_log_file" ]]; then
    exec > >(tee -a "$control_log_file") 2>&1
  fi

  if run_attached "$@"; then
    exit_code=0
  else
    exit_code=$?
  fi

  if [[ -n "$control_exit_code_file" ]]; then
    printf '%s\n' "$exit_code" >"$control_exit_code_file"
  fi

  exit "$exit_code"
fi

if ((no_flicker == 1 && internal_launch == 0)); then
  run_no_flicker "$@"
  exit 0
fi

run_attached "$@"
exit $?
