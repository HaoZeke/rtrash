# bash completion for rtrash and multi-call names
# shellcheck shell=bash
# Preferred install (embedded assets, no source tree):
#   rtrash setup
# Or: rtrash completions bash > …/bash-completion/completions/rtrash

_rtrash_put_opts=(
  -f --force -i -I --interactive
  -r -R --recursive -d --dir -v --verbose
  --one-file-system --preserve-root --no-preserve-root
  --trash-dir= --home-only --plain
  --help --version
)

_rtrash_empty_opts=(
  -n --dry-run -v --verbose -f --force
  --trash-dir= --home-only --plain --older-than= --json
  --help --version
)

_rtrash_list_opts=(
  --home-only --trash-dir= --older-than= --newer-than= --json --help --version
)

_rtrash_status_opts=(
  --home-only --trash-dir= --older-than= --newer-than= --json --help --version
)

_rtrash_restore_opts=(
  -f --force --home-only --trash-dir= --help --version
)

_rtrash_rm_opts=(
  --trash-dir= --home-only -n --dry-run -f --force -v --verbose
  --older-than= --newer-than= --json
  --help --version
)

_rtrash_subcommands=(put empty list status restore rm setup completions man)

_rtrash_setup_opts=(
  --prefix= --bin-dir= --with-rm -n --dry-run -f --force -v --verbose --help
)

_rtrash_complete_from() {
  local cur="${COMP_WORDS[COMP_CWORD]}"
  local -n _opts=$1
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
      _rtrash_complete_from _rtrash_restore_opts
      return
      ;;
    trash-rm)
      _rtrash_complete_from _rtrash_rm_opts
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
      put | empty | list | status | restore | rm)
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
    restore) _rtrash_complete_from _rtrash_restore_opts ;;
    rm) _rtrash_complete_from _rtrash_rm_opts ;;
    setup) _rtrash_complete_from _rtrash_setup_opts ;;
    completions)
      if [[ $cur == -* ]]; then
        COMPREPLY=($(compgen -W "--help" -- "$cur"))
      else
        COMPREPLY=($(compgen -W "bash zsh" -- "$cur"))
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
