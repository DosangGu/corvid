if (( ${+CORVID_ORIGINAL_ZDOTDIR} )); then
    export ZDOTDIR=$CORVID_ORIGINAL_ZDOTDIR
else
    unset ZDOTDIR
fi
unset CORVID_ORIGINAL_ZDOTDIR

if [[ "${ZDOTDIR-$HOME}" != "$CORVID_SHELL_COMMANDS" && -r "${ZDOTDIR-$HOME}/.zshenv" ]]; then
    source "${ZDOTDIR-$HOME}/.zshenv"
fi

if [[ -o interactive ]]; then
    function _corvid_shell_agent {
        local provider=$1
        shift
        local wrapper="$CORVID_SHELL_COMMANDS/$provider"
        if [[ -x "$wrapper" ]]; then
            command "$wrapper" "$@"
        else
            command "$provider" "$@"
        fi
    }

    for _corvid_provider in @CORVID_PROVIDERS@; do
        if (( ! ${+functions[$_corvid_provider]} )); then
            functions[$_corvid_provider]="_corvid_shell_agent $_corvid_provider \"\$@\""
        fi
    done
    unset _corvid_provider
fi
