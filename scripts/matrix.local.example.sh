# Machine-specific settings for scripts/run_matrix.sh. Copy this file to scripts/matrix.local.sh (which
# is gitignored, so routes and user names stay out of the repository) and fill it in. run_matrix.sh
# sources it before reading its settings; anything set in the environment still wins.

# The laptop column: a second Windows machine reached over ssh, with Git for Windows installed there.
: "${PE_LAPTOP_SSH:=ssh -o BatchMode=yes -o LogLevel=ERROR -o ConnectTimeout=15 -p 22 -i $HOME/.ssh/<key> <user>@<host>}"
: "${PE_LAPTOP_DIR:=C:/Users/<user>/pe-matrix}"

# Optional: a wrapper every local cargo command and WSL job runs through (`bash $PE_HEAVY <command>`).
# : "${PE_HEAVY:=/path/to/heavy.sh}"
