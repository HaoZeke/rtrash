# elvish completion for rtrash and its multi-call names
# Preferred install: rtrash setup (into PREFIX/share/elvish/lib)
# Or: rtrash completions elvish > ~/.config/elvish/lib/rtrash.elv
# Then add `use rtrash` to ~/.config/elvish/rc.elv.

use path
use str

var subs = [put empty list status trash-stats du restore undo rm who reap finder config setup keys completions man]
var opts = [
  &put=[-f --force -i -I --interactive -r -R --recursive -d --dir -v --verbose --one-file-system --preserve-root --no-preserve-root --trash-dir= --home-only --plain --no-space-check --in-use= --json --help --version]
  &empty=[-n --dry-run -v --verbose -f --force -i --interactive --all-users --plain --older-than= --max-size= --prune= --dangling --json --trash-dir= --home-only --help --version]
  &list=[--home-only --trash-dir= --all-users --volumes --trash-dirs --older-than= --newer-than= --larger-than= --smaller-than= --size --sort= --reverse --json --help --version]
  &status=[--home-only --trash-dir= --older-than= --newer-than= --larger-than= --smaller-than= --json --help --version]
  &trash-stats=[--top= --home-only --trash-dir= --history --since= --total --measure --space --json --help --version]
  &du=[-n --top= -x --one-file-system -L --follow-symlinks --apparent --sort= -C --count --inodes --types --unique --shared --max-depth= --threads= --all-dirs --artefacts --exclude= -X --exclude-from= --exclude-caches --gitignore --dirs --files --older-than= --newer-than= --since= --until= --home-only --trash-dir= --no-git --include-dirty --json --print0 --dupes --trash-dupes --link-dupes --clone-dupes --min-size= -o --output= --devices --ncdu= --ncdu-binary= -f --file= --stacks --pick -i --interactive --no-delete --si --help --version]
  &restore=[-f --force --overwrite --sort= --conflict= --to= --cwd-only --plain --no-space-check --newest-first --oldest-first --name= --glob= --regex= --latest --all --json --home-only --trash-dir= --help --version]
  &undo=[-l --list -n --dry-run -f --force --conflict= --json --home-only --trash-dir= --help --version]
  &rm=[--trash-dir= --home-only --older-than= --newer-than= --larger-than= --smaller-than= -n --dry-run -f --force -v --verbose --json --help --version]
  &who=[--json --help --version]
  &reap=[-p --profile= --list-profiles --roots= --depth= --tool --older-than= --age-by= -n --dry-run --permanent --trash --trash-dir= --home-only -v --verbose --shared --install-timer --watch --watch= --remove-timer --timer-status --when-below= --purge-trash --until= --json --help --version]
  &finder=[--trash-dir= --all -f --force --conflict= -v --verbose --json --help]
  &config=[--path --default --help --version]
  &setup=[--prefix= --bin-dir= --with-rm --powershell-profile -n --dry-run -f --force -v --verbose --help]
  &keys=[--list -l --sample --path --help]
  &completions=[]
  &man=[]
]
var multi = [&trash=put &trash-put=put &trash-empty=empty &trash-list=list &trash-restore=restore &trash-rm=rm]
var values = [&completions=[bash zsh fish nu powershell elvish] &finder=[put list restore]]

fn complete {|@words|
  var cmd = (path:base $words[0])
  var cur = $words[-1]
  var sub = ''
  if (has-key $multi $cmd) {
    set sub = $multi[$cmd]
  } else {
    for w $words[1..-1] {
      if (has-value $subs $w) {
        set sub = $w
        break
      }
    }
  }
  var cands = []
  if (eq $sub '') {
    # Bare rtrash: a subcommand, or the put options it falls through to.
    if (str:has-prefix $cur -) {
      set cands = [$@opts[put] -h -V]
    } else {
      set cands = $subs
    }
  } elif (str:has-prefix $cur -) {
    set cands = $opts[$sub]
  } elif (has-key $values $sub) {
    set cands = $values[$sub]
  }
  for c $cands {
    if (str:has-prefix $c $cur) {
      put $c
    }
  }
}

# Register with the line editor (interactive shells only).
# Register with the line editor (interactive shells only).
set edit:completion:arg-completer[rtrash] = $complete~
for name [(keys $multi)] {
  set edit:completion:arg-completer[$name] = $complete~
}
