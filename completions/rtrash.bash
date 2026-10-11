# bash completion for rtrash and multi-call names
# shellcheck shell=bash
# Preferred install (embedded assets, no source tree):
#   rtrash setup
# Or: rtrash completions bash > …/bash-completion/completions/rtrash

_rtrash_put_opts=(
  -f --force -i -I --interactive
  -r -R --recursive -d --dir -v --verbose
  --one-file-system --preserve-root --preserve-root=all --no-preserve-root
  --trash-dir= --home-only --plain --json
  --help --version
)

_rtrash_empty_opts=(
  -n --dry-run -v --verbose -f --force
  --trash-dir= --home-only --plain --older-than= --json
  --max-size= --prune=
  --help --version
)

_rtrash_list_opts=(
  --home-only --trash-dir= --older-than= --newer-than= --json
  --larger-than= --smaller-than= --size --sort= --reverse --all-users --help --version
)

_rtrash_status_opts=(
  --home-only --trash-dir= --older-than= --newer-than= --json
  --larger-than= --smaller-than= --help --version
)

_rtrash_du_opts=(
  -n --top= -x --one-file-system --apparent --sort= -C --count --unique --shared --types --max-depth= --threads= --all-dirs
  --artefacts --exclude= -X --exclude-from= --exclude-caches --gitignore
  --dirs --files --older-than= --newer-than= --since= --until= --home-only
  --trash-dir= --no-git --include-dirty --json --print0 --pick -i --interactive --no-delete --si -o --output= --devices --ncdu= --ncdu-binary= -f --file= --stacks --help
  --version
)

_rtrash_restore_opts=(
  -f --force --home-only --trash-dir= --cwd-only --plain
  --newest-first --oldest-first --name= --glob= --regex= --latest --all
  --to= --conflict= --json --help --version
)

_rtrash_rm_opts=(
  --trash-dir= --home-only -n --dry-run -f --force -v --verbose
  --older-than= --newer-than= --larger-than= --smaller-than= --json
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

_rtrash_config_opts=(
  --path --default --help --version
)

_rtrash_subcommands=(put empty list status du restore undo rm finder config setup keys completions man)

_rtrash_keys_opts=(
  -l --list --sample --path --help
)

_rtrash_setup_opts=(
  --prefix= --bin-dir= --with-rm -n --dry-run -f --force -v --verbose --help
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
      put | empty | list | status | du | restore | undo | rm | finder | config | setup | keys | completions | man)
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
    du) _rtrash_complete_from _rtrash_du_opts ;;
    restore) _rtrash_complete_from _rtrash_restore_opts ;;
    undo) _rtrash_complete_from _rtrash_undo_opts ;;
    finder) _rtrash_complete_from _rtrash_finder_opts ;;
    config) _rtrash_complete_from _rtrash_config_opts ;;
    rm) _rtrash_complete_from _rtrash_rm_opts ;;
    setup) _rtrash_complete_from _rtrash_setup_opts ;;
    keys) COMPREPLY=($(compgen -W "${_rtrash_keys_opts[*]}" -- "$cur")) ;;
    completions)
      if [[ $cur == -* ]]; then
        COMPREPLY=($(compgen -W "--help" -- "$cur"))
      else
        COMPREPLY=($(compgen -W "bash zsh fish" -- "$cur"))
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
