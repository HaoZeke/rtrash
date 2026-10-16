# bash completion for rtrash and multi-call names
# shellcheck shell=bash
# Preferred install (embedded assets, no source tree):
#   rtrash setup
# Or: rtrash completions bash > …/bash-completion/completions/rtrash

_rtrash_put_opts=(
  -f --force -i -I --interactive
  -r -R --recursive -d --dir -v --verbose
  --one-file-system --preserve-root --preserve-root=all --no-preserve-root
  --trash-dir= --home-only --plain --no-space-check --dry-run --in-use= --json
  --help --version
)

_rtrash_empty_opts=(
  -n --dry-run -v --verbose -f --force -i --interactive --all-users
  --trash-dir= --home-only --plain --older-than= --json
  --max-size= --prune= --until-free= --dangling --all
  --help --version
)

_rtrash_list_opts=(
  --home-only --trash-dir= --older-than= --newer-than= --json
  --larger-than= --smaller-than= --size --sort= --reverse --glob= --regex= --name= --cwd-only --format= --all-users --volumes --trash-dirs --help --version
)

_rtrash_trash_stats_opts=(
  --top= --home-only --trash-dir= --history --since= --total --measure --space --exit-low --json --help --version
)

_rtrash_status_opts=(
  --home-only --trash-dir= --older-than= --newer-than= --json
  --larger-than= --smaller-than= --help --version
)

_rtrash_du_opts=(
  -n --top= -x --one-file-system -L --follow-symlinks --apparent --sort= -C --count --inodes --unique --shared --types --max-depth= --threads= --all-dirs
  --artefacts --exclude= -X --exclude-from= --exclude-caches --gitignore
  --dirs --files --older-than= --newer-than= --since= --until= --home-only
  --trash-dir= --no-git --include-dirty --json --print0 --dupes --trash-dupes --link-dupes --clone-dupes --min-size= --pick -i --interactive --no-delete --si -o --output= --devices --ncdu= --ncdu-binary= -f --file= --stacks --help
  --version
)

_rtrash_restore_opts=(
  -f --force --home-only --trash-dir= --cwd-only --plain --no-space-check
  --newest-first --oldest-first --overwrite --sort= --name= --glob= --regex= --latest --all --batch=
  -n --dry-run --to= --conflict= --json --help --version
)

_rtrash_rm_opts=(
  --trash-dir= --home-only -n --dry-run -f --force -v --verbose
  --older-than= --newer-than= --larger-than= --smaller-than= --batch= --json
  --help --version
)

_rtrash_undo_opts=(
  -l --list -n --dry-run -f --force --conflict= --json --home-only --trash-dir=
  --help --version
)

_rtrash_finder_opts=(
  put list restore --trash-dir= --all -f --force --conflict= -v --verbose
  --json --help
)

_rtrash_wsl_opts=(
  list restore rm empty --older-than= --all --json --format= --glob= --regex= --name= -n --dry-run -f --force --help
)

_rtrash_who_opts=(
  --json --help --version
)

_rtrash_reap_opts=(
  -p --profile= --list-profiles --roots= --depth= --tool --trash --older-than= --age-by= -n --dry-run --permanent --trash-dir= --home-only -v --verbose --shared --install-timer --install-timer= --watch --watch= --remove-timer --timer-status --when-below= --until= --purge-trash --json --help --version
)

_rtrash_config_opts=(
  --path --default --help --version
)

_rtrash_subcommands=(put empty list status trash-stats du restore undo rm who reap finder wsl config setup keys completions man)

_rtrash_keys_opts=(
  -l --list --sample --path --help
)

_rtrash_setup_opts=(
  --prefix= --bin-dir= --with-rm --powershell-profile --powershell-profile= -n --dry-run -f --force -v --verbose --help
)

_rtrash_complete_from() {
  local cur="${COMP_WORDS[COMP_CWORD]}"
  # Copy the named array; bash 3.2 (macOS /bin/bash) has no namerefs.
  local -a _opts
  eval "_opts=(\"\${${1}[@]}\")"
  # Prefer option completion when cur starts with -
  if [[ $cur == -* ]]; then
    COMPREPLY=($(compgen -W "${_opts[*]}" -- "$cur"))
    return
  fi
  # Otherwise complete files (and options as fallback for bare flags later)
  COMPREPLY=($(compgen -f -- "$cur"))
  local o
  for o in "${_opts[@]}"; do
    [[ $o == "$cur"* ]] && COMPREPLY+=("$o")
  done
}

# Trashed items whose FIELD (path or name) starts with PREFIX, from
# `rtrash list` (or trash-list when only the multi-call names exist).
_rtrash_trashed() {
  local field=$1 prefix=$2 p
  local -a lister=(rtrash list)
  command -v rtrash >/dev/null 2>&1 || lister=(trash-list)
  while IFS= read -r -d '' p; do
    [[ $p == "$prefix"* ]] && COMPREPLY+=("$(printf '%q' "$p")")
  done < <(command "${lister[@]}" --format="{$field}\\0" 2>/dev/null)
}

# restore: options, names after --name=, otherwise trashed original paths.
_rtrash_complete_restore() {
  local cur="${COMP_WORDS[COMP_CWORD]}" i=$COMP_CWORD
  COMPREPLY=()
  # Readline splits --name=x at the '=' (COMP_WORDBREAKS).
  if [[ $cur == = && ${COMP_WORDS[i - 1]} == --name ]]; then
    _rtrash_trashed name ""
    return
  fi
  if ((i > 1)) && [[ ${COMP_WORDS[i - 1]} == = && ${COMP_WORDS[i - 2]} == --name ]]; then
    _rtrash_trashed name "$cur"
    return
  fi
  case "$cur" in
    --name=*) _rtrash_trashed name "${cur#--name=}" ;;
    -*) COMPREPLY=($(compgen -W "${_rtrash_restore_opts[*]}" -- "$cur")) ;;
    *) _rtrash_trashed path "$cur" ;;
  esac
}

# Recycle Bin items whose path under the automount root (path) or file
# name (name) starts with PREFIX, from `rtrash wsl list`.
_rtrash_wsl_items() {
  local field=$1 prefix=$2 p
  while IFS= read -r -d '' p; do
    [[ $field == name ]] && p=${p##*/}
    [[ $p == "$prefix"* ]] && COMPREPLY+=("$(printf '%q' "$p")")
  done < <(command rtrash wsl list --format='{path}\0' 2>/dev/null)
}

# wsl: file names after --name=; after restore or rm, Recycle Bin items
# by their path under the automount root; otherwise actions, options
# and files.
_rtrash_complete_wsl() {
  local cur="${COMP_WORDS[COMP_CWORD]}" i=$COMP_CWORD act=
  COMPREPLY=()
  # Readline splits --name=x at the '=' (COMP_WORDBREAKS).
  if [[ $cur == = && ${COMP_WORDS[i - 1]} == --name ]]; then
    _rtrash_wsl_items name ""
    return
  fi
  if ((i > 1)) && [[ ${COMP_WORDS[i - 1]} == = && ${COMP_WORDS[i - 2]} == --name ]]; then
    _rtrash_wsl_items name "$cur"
    return
  fi
  if [[ $cur == --name=* ]]; then
    _rtrash_wsl_items name "${cur#--name=}"
    return
  fi
  for ((i = 1; i < COMP_CWORD; i++)); do
    [[ ${COMP_WORDS[i - 1]} == wsl ]] && act=${COMP_WORDS[i]} && break
  done
  if [[ ($act == restore || $act == rm) && $cur != -* ]]; then
    _rtrash_wsl_items path "$cur"
    return
  fi
  _rtrash_complete_from _rtrash_wsl_opts
}

# rm: options, otherwise trashed original paths (a path is a pattern).
_rtrash_complete_rm() {
  local cur="${COMP_WORDS[COMP_CWORD]}"
  COMPREPLY=()
  case "$cur" in
    -*) COMPREPLY=($(compgen -W "${_rtrash_rm_opts[*]}" -- "$cur")) ;;
    *) _rtrash_trashed path "$cur" ;;
  esac
}

_rtrash_main() {
  local cur prev words cword
  # Prefer bash-completion helper when available
  if declare -F _init_completion >/dev/null 2>&1; then
    _init_completion -n = || return
  else
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD - 1]}"
  fi

  local cmd="${COMP_WORDS[0]##*/}"

  case "$cmd" in
    trash-empty)
      _rtrash_complete_from _rtrash_empty_opts
      return
      ;;
    trash-list)
      _rtrash_complete_from _rtrash_list_opts
      return
      ;;
    trash-restore)
      _rtrash_complete_restore
      return
      ;;
    trash-rm)
      _rtrash_complete_rm
      return
      ;;
    rm | trash | trash-put)
      _rtrash_complete_from _rtrash_put_opts
      return
      ;;
  esac

  # rtrash binary: subcommand or put-style bare invocation
  local i sub=
  for ((i = 1; i < ${#COMP_WORDS[@]}; i++)); do
    case "${COMP_WORDS[i]}" in
      put | empty | list | status | trash-stats | du | restore | undo | rm | who | reap | finder | wsl | config | setup | keys | completions | man)
        sub="${COMP_WORDS[i]}"
        break
        ;;
      -h | --help | -V | --version | help)
        return
        ;;
    esac
  done

  if [[ -z $sub ]]; then
    # First arg: subcommands, global flags, or files (put fallthrough)
    if [[ $cur == -* ]]; then
      COMPREPLY=($(compgen -W "-h --help -V --version ${_rtrash_put_opts[*]}" -- "$cur"))
    else
      COMPREPLY=($(compgen -W "${_rtrash_subcommands[*]}" -- "$cur"))
      COMPREPLY+=($(compgen -f -- "$cur"))
    fi
    return
  fi

  case "$sub" in
    put) _rtrash_complete_from _rtrash_put_opts ;;
    empty) _rtrash_complete_from _rtrash_empty_opts ;;
    list) _rtrash_complete_from _rtrash_list_opts ;;
    status) _rtrash_complete_from _rtrash_status_opts ;;
    trash-stats) _rtrash_complete_from _rtrash_trash_stats_opts ;;
    du) _rtrash_complete_from _rtrash_du_opts ;;
    restore) _rtrash_complete_restore ;;
    undo) _rtrash_complete_from _rtrash_undo_opts ;;
    who) _rtrash_complete_from _rtrash_who_opts ;;
    reap) _rtrash_complete_from _rtrash_reap_opts ;;
    finder) _rtrash_complete_from _rtrash_finder_opts ;;
    wsl) _rtrash_complete_wsl ;;
    config) _rtrash_complete_from _rtrash_config_opts ;;
    rm) _rtrash_complete_rm ;;
    setup) _rtrash_complete_from _rtrash_setup_opts ;;
    keys) COMPREPLY=($(compgen -W "${_rtrash_keys_opts[*]}" -- "$cur")) ;;
    completions)
      if [[ $cur == -* ]]; then
        COMPREPLY=($(compgen -W "--help" -- "$cur"))
      else
        COMPREPLY=($(compgen -W "bash zsh fish nu powershell elvish" -- "$cur"))
      fi
      ;;
    man)
      COMPREPLY=($(compgen -W "--help" -- "$cur"))
      ;;
  esac
}

complete -F _rtrash_main rtrash
complete -F _rtrash_main trash-put trash-empty trash-list trash-restore trash-rm trash
# Optional: only register rm if the user wants rtrash as rm. Many systems use
# GNU rm; leave commented. Uncomment to complete multi-call rm → put:
# complete -F _rtrash_main rm
