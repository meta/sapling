# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Maintained by Ryan McElroy <rm@fb.com>
#
# Inspiration and derivation from git-completion.bash by Shawn O. Pearce.
#
# ========================================================================
#
# Quickly determines the and emits some useful information about the state
# of your current mercurial or git repository. Useful for PS1 prompts.
#
# Design goals:
#  * Useful for both git and mercurial
#  * Portable to both zsh and bash
#  * Portable to both Mac (BSD-based utils) and Linux (GNU-based utils)
#  * As fast as possible given the above constraints (few command invocations)
#  * Avoids invoking git or mercurial, which may be slow on large repositories
#
# To use from zsh:
#
#   NOTE! the single quotes are important; if you use double quotes
#   then the prompt won't change when you chdir or checkout different
#   branches!
#
#   setopt PROMPT_SUBST
#   source /path/to/scm-prompt
#   export PS1='$(_scm_prompt)$USER@%m:%~%% '
#
# To use from bash:
#
#   source /path/to/scm-prompt
#   export PS1="\$(_scm_prompt)\u@\h:\W\$ "
#
#   NOTE! You *EITHER* need to single-quote the whole thing *OR* back-slash
#   the $(...) (as above), but not both. Which one you use depends on if
#   you need the rest of your PS1 to interpolate variables.
#
# You may additionally pass a format-string to the scm_info command. This
# allows you to control the format of the prompt string without interfering
# with the prompt outside of a mercurial or git repository. For example:
#
#   $(_scm_prompt "%s")
#
# The default format string is " (%s)" (note the space)
#
# Options: you may want to set these environment variables
#  * HOME_IS_NOT_A_REPO : Stop walking up the filesystem looking for a git or
#    hg repo at (but not including) /home instead of /.  This helps when /home
#    is a remote or autofs mount point.
#  * WANT_OLD_SCM_PROMPT : Use '%s' as the formatting for the prompt instead
#    of ' (%s)'
#  * SHOW_DIRTY_STATE : Show dirty state of the working directory of a
#    repo with a star *. In addition, for hg, show untracked files with ?.
#    For git, show staged change with +.  When SHOW_DIRTY_STATE is set,
#    you can opt out individual repo by setting shell.showDirtyState to false
#    in .hg/hgrc or .git/config.
#  * SCM_PROMPT_SHOW_WORKTREE : Append the worktree's name to the prompt.
#    For git, read `sl/worktreename` under the resolved gitdir, falling back
#    to the gitdir basename for linked worktrees. For sl/hg, read
#    `.sl/worktreename` (or `.hg/worktreename`). `sl worktree add` and
#    `sl worktree label` write these markers.
#
# Notes to developers:
#
#  * Aliases can screw up the default commands. To prevent this issue, use
#    the 'builtin' prefix for built-in shell commands (eg, 'cd' and 'echo')
#    and use the 'command' prefix for external commands that you do not want
#    to invoke aliases for (eg, 'grep', 'cut').
#
# =========================================================================
#

_find_most_relevant() {
    # We don't want to output all remote bookmarks because there can be many
    # of them. This function finds the most relevant remote bookmark using this
    # algorithm:
    # 1. If 'master' or '@' bookmark is available then output it
    # 2. Sort remote bookmarks and output the first in reverse sorted order (
    # it's a heuristic that tries to find the newest bookmark. It will work well
    # with bookmarks like 'release20160926' and 'release20161010').
    relevantbook="$(command grep -m1 -E -o "^[^/]+/(master|@)$" <<< "$1")"
    if [[ -n $relevantbook ]]; then
        builtin echo $relevantbook
        return 0
    fi

    builtin echo "$(command sort -r <<< "$1" | command head -n 1)"
}

_hg_prompt() {
  local hg br extra
  hg="$1"

  if [[ -f "$hg/bisect.state" ]]; then
    extra="|BISECT"
  elif [[ -f "$hg/histedit-state" ]]; then
    extra="|HISTEDIT"
  elif [[ -f "$hg/graftstate" ]]; then
    extra="|GRAFT"
  elif [[ -f "$hg/dropstate" ]]; then
    extra="|DROP"
  elif [[ -f "$hg/unshelverebasestate" ]]; then
    extra="|UNSHELVE"
  elif [[ -f "$hg/rebasestate" ]]; then
    extra="|REBASE"
  elif [[ -d "$hg/merge" ]]; then
    extra="|MERGE"
  elif [[ -L "$hg/store/lock" ]]; then
    extra="|STORE-LOCKED"
  elif [[ -L "$hg/wlock" ]]; then
    extra="|WDIR-LOCKED"
  fi

  local dirstate="$( \
    ( [[ -f "$hg/dirstate" ]] && \
    command hexdump -vn 20 -e '1/1 "%02x"' "$hg/dirstate") || \
    builtin echo "empty")"

  local shared_hg="$hg"
  if [[ -f "$hg/sharedpath" ]]; then
    shared_hg="$(command cat "$hg/sharedpath")"
    # `relshared` shares store a path relative to $hg, not absolute.
    if [[ "$shared_hg" != /* ]]; then
      shared_hg="$hg/$shared_hg"
    fi
  fi
  local remote="$shared_hg/store/remotenames"

  local active="$hg/bookmarks.current"
  if  [[ -f "$active" ]]; then
    br="$(command cat "$active")"
    # check to see if active bookmark needs update (eg, moved after pull)
    local marks="$shared_hg/store/bookmarks"
    if [[ -z "$extra" ]] && [[ -f "$marks" ]]; then
      local markstate="$(command grep " $br$" "$marks" | \
        command cut -f 1 -d ' ')"
      if [[ $markstate != "$dirstate" ]]; then
        extra="|UPDATE_NEEDED"
      fi
    fi
  else
    br="$(builtin echo "$dirstate" | command cut -c 1-10)"
  fi
  if [[ -n "$SCM_PROMPT_SHOW_WORKTREE" ]] && [[ -f "$hg/worktreename" ]]; then
    local wtname="$(command cat "$hg/worktreename")"
    [[ -n "$wtname" ]] && br="$br|$wtname"
  fi
  if [[ -f "$remote" ]]; then
    local allremotemarks="$(command grep "^$dirstate bookmarks" "$remote" | \
      command cut -f 3 -d ' ')"

    if [[ -n "$allremotemarks" ]]; then
        local remotemark="$(_find_most_relevant "$allremotemarks")"
        if [[ -n "$remotemark" ]]; then
          br="$br|$remotemark"
          if [[ "$remotemark" != "$allremotemarks" ]]; then
            # if there is more than one, let the user know with an ellipsis
            br="${br}..."
          fi
        fi
    fi
  fi
  local branch
  if [[ -f "$hg/branch" ]]; then
    branch="$(command cat "$hg/branch")"
    if [[ "$branch" != "default" ]]; then
      br="$br|$branch"
    fi
  fi
  br="$br$extra$(_hg_dirty)"
  builtin printf "%s" "$br"
}

# cf. https://www.internalfb.com/intern/qa/4964/how-do-i-show-the-mercurial-bookmarkgit-branch-in?answerID=540621130023036
_hg_dirty() {
  if [ -n "${SHOW_DIRTY_STATE}" ] &&
     [ "$(command sl config shell.showDirtyState)" != "false" ]; then
       command sl status 2> /dev/null \
         | command awk '{print $1}'\
         | command sort\
         | command uniq\
         | command paste -sd ' ' -
  fi
}

_git_dirty() {
  # cf. git contrib/completion/git-prompt.sh
  if [ -n "${SHOW_DIRTY_STATE}" ] &&
     [ "$(git config --bool shell.showDirtyState)" != "false" ]; then
       command git diff --no-ext-diff --quiet || w="*"
       command git diff --no-ext-diff --cached --quiet || i="+"
       builtin printf "%s" "$w$i"
  fi
}

_git_prompt() {
  local git br wtname
  git="$1"
  if [[ -f "$git" ]]; then
      local gitfile="$git"
      git="$(command awk '/^gitdir:/ { sub(/^gitdir:[[:space:]]*/, ""); print; exit }' < "$gitfile")"
      [[ -z "$git" ]] && return
      if [[ "$git" != /* ]]; then
        git="${gitfile%/*}/$git"
      fi
      if [[ -n "$SCM_PROMPT_SHOW_WORKTREE" ]]; then
        # Linked worktrees store per-worktree state directly under
        # /main/.git/worktrees/<name>; other .git pointers include submodules.
        case "$git" in
          */.git/worktrees/*/*|.git/worktrees/*/*) ;;
          */.git/worktrees/*|.git/worktrees/*)
            wtname="$(command basename "$git")"
            ;;
        esac
      fi
  fi
  if [[ -n "$SCM_PROMPT_SHOW_WORKTREE" ]] && [[ -f "$git/sl/worktreename" ]]; then
    local label="$(command cat "$git/sl/worktreename")"
    [[ -n "$label" ]] && wtname="$label"
  fi
  if [[ -f "$git/HEAD" ]]; then
    read br < "$git/HEAD"
    if [[ "$br" = "ref: refs/heads/.invalid" ]]; then
      # A reftable repo keeps this placeholder in HEAD and stores the real
      # value in the reftable, which only git can read.
      br="$(command git --git-dir="$git" symbolic-ref --quiet HEAD 2>/dev/null \
        || command git --git-dir="$git" rev-parse HEAD 2>/dev/null)"
      [[ "$br" = refs/* ]] && br="ref: $br"
    fi
    case $br in
      ref:\ refs/heads/*) br=${br#ref: refs/heads/} ;;
      *) br="$(builtin echo "$br" | command cut -c 1-8)" ;;
    esac
    [[ -n "$wtname" ]] && br="$br|$wtname"
    if [[ -f "$git/rebase-merge/interactive" ]]; then
      b="$(command cat "$git/rebase-merge/head-name")"
      b="${b#refs/heads/}"
      br="$br|REBASE-i|$b"
    elif [[ -d "$git/rebase-merge" ]]; then
      b="$(command cat "$git/rebase-merge/head-name")"
      b="${b#refs/heads/}"
      br="$br|REBASE-m|$b"
    else
      if [[ -d "$git/rebase-apply" ]]; then
        if [[ -f "$git/rebase-apply/rebasing" ]]; then
          b="$(command cat "$git/rebase-apply/head-name")"
          b="${b#refs/heads/}"
          br="$br|REBASE|$b"
        elif [[ -f "$git/rebase-apply/applying" ]]; then
          br="$br|AM"
        else
          br="$br|AM/REBASE"
        fi
      elif [[ -f "$git/CHERRY_PICK_HEAD" ]]; then
        br="$br|CHERRY-PICKING"
      elif [[ -f "$git/REVERT_HEAD" ]]; then
        br="$br|REVERTING"
      elif [[ -f "$git/MERGE_HEAD" ]]; then
        br="$br|MERGE"
      elif [[ -f "$git/BISECT_LOG" ]]; then
        br="$br|BISECT"
      fi
    fi
  fi
  br="$br$(_git_dirty)"
  builtin printf "%s" "$br"
}

_scm_prompt() {
  local dir fmt br
  # Default to be compatible with __git_ps1. In particular:
  # - provide a space for the user so that they don't have to have
  #   random extra spaces in their prompt when not in a repo
  # - provide parens so it's differentiated from other crap in their prompt
  fmt="${1:- (%s)}"

  # find out if we're in a git or hg repo by looking for the control dir
  dir="$PWD"
  while : ; do
    [[ -n "$HOME_IS_NOT_A_REPO" ]] && [[ "$dir" = "/home" ]] && break
    if [[ "${SCM_PROMPT_PREFER_GIT:-1}" = 1 ]] && [[ -r "$dir/.git" ]]; then
      br="$(_git_prompt "$dir/.git")"
      break
    elif [[ -d "$dir/.hg" ]]; then
      br="$(_hg_prompt "$dir/.hg")"
      break
    elif [[ -d "$dir/.sl" ]]; then
      br="$(_hg_prompt "$dir/.sl")"
      break
    elif [[ -d "$dir/.git/sl" ]]; then
      br="$(_hg_prompt "$dir/.git/sl")"
      break
    elif [[ -r "$dir/.git" ]]; then
      br="$(_git_prompt "$dir/.git")"
      break
    fi
    [[ "$dir" = "/" ]] && break
    # portable "realpath" equivalent
    dir="$(chpwd_functions=(); chpwd() { :; }; builtin cd -P "$dir/.." >/dev/null && builtin echo "$PWD")"
  done

  if [[ -n "$br" ]]; then
    builtin printf "$fmt" "$br"
  fi
}

#
# Backwards-compatibility layer for odl scm-prompt script
#
# Older versions of this file at Facebook used a longer function name.
# These versions also included support for an environmental directive
# called $WANT_OLD_SCM_PROMPT. Support this to remain compatible.
#

_dotfiles_scm_info() {
  local fmt
  fmt=$1
  if [[ -z "$fmt" ]]; then
    if [[ -n "$WANT_OLD_SCM_PROMPT" ]]; then
      fmt="%s"
    else
      fmt=' (%s)'
    fi
  fi
  _scm_prompt "$fmt"
}
