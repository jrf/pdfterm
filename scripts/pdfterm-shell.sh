# Source from a local Bash/Zsh interactive startup file after adding scripts/ to PATH.
# PDFTERM_SSH_HOSTS is a space-separated list of SSH aliases to enable.
ssh() {
    case $- in
        *i*)
            if [ "$#" -eq 1 ] && [ -t 0 ] && [ -t 1 ] &&
                [ -z "${SSH_CONNECTION-}${SSH_TTY-}${TMUX-}" ]; then
                case " ${PDFTERM_SSH_HOSTS-} " in
                    *" $1 "*) command pdfterm-ssh "$1"; return $? ;;
                esac
            fi
            ;;
    esac
    command ssh "$@"
}
