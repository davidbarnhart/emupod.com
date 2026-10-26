#!/bin/sh
# emupod installer: sets up the emupod server on a Raspberry Pi or a Linux PC, so you can use the iPod from your phone's browser.
#
#   curl -fsSL https://emupod.com/install.sh | sh
#
# It installs one program into your home directory (no root needed, except for two optional extras: the card-burning permission and
# the service that mounts the emulated iPod's disk), downloads nothing
# from Apple until you ask for a firmware, and writes nothing to any iPod or card. Read it before you run it: it is a short shell script.
#
# Environment: EMUPOD_VERSION (default: the latest release), EMUPOD_HOME (default ~/emupod), EMUPOD_PORT (default 8797), EMUPOD_YES=1 (accept every default).
set -eu

REPO="davidbarnhart/emupod.com"
HOME_DIR="${EMUPOD_HOME:-$HOME/emupod}"
say() { printf '%s\n' "$*"; }        # a line for you: a question, a problem, something to do (flush left)
note() { printf '  %s\n' "$*"; }     # progress, nothing to do (indented)
die() { printf 'emupod install: %s\n' "$*" >&2; exit 1; }
ask() {  # ask "question" default -> answer in $ANSWER
    if [ "${EMUPOD_YES:-}" = 1 ] || [ ! -r /dev/tty ]; then ANSWER="$2"; return; fi
    printf '%s [%s] ' "$1" "$2" > /dev/tty; read -r ANSWER < /dev/tty || ANSWER=""; [ -n "$ANSWER" ] || ANSWER="$2"
}
yesno() {  # yesno "question" Y|N: asks "question? Y/n" (the capital is what Enter gives); the answer, Y or N, in $ANSWER
    if [ "${EMUPOD_YES:-}" = 1 ] || [ ! -r /dev/tty ]; then ANSWER="$2"; return; fi
    if [ "$2" = Y ]; then printf '%s Y/n ' "$1" > /dev/tty; else printf '%s y/N ' "$1" > /dev/tty; fi
    read -r ANSWER < /dev/tty || ANSWER=""
    case "$ANSWER" in y*|Y*) ANSWER=Y ;; n*|N*) ANSWER=N ;; *) ANSWER="$2" ;; esac
}
port_busy() { command -v ss >/dev/null 2>&1 && ss -ltn 2>/dev/null | awk '{print $4}' | grep -qE "[:.]$1\$"; }

# 1. the machine
[ "$(uname -s)" = Linux ] || die "this installer is for Linux (a Raspberry Pi or a PC); you are on $(uname -s)."
case "$(uname -m)" in
    x86_64|amd64) ARCH=x86_64 ;;
    aarch64|arm64) ARCH=aarch64 ;;
    *) die "unsupported processor $(uname -m): emupod needs a 64-bit machine (x86-64, or an ARM board running a 64-bit OS; on a Raspberry Pi use 'Raspberry Pi OS (64-bit)')." ;;
esac
command -v curl >/dev/null 2>&1 || die "curl is missing: sudo apt install curl"
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is missing (it is part of coreutils)."

RULE=/etc/udev/rules.d/70-emupod-sd.rules
CAN_BURN=""; command -v udevadm >/dev/null 2>&1 && command -v sudo >/dev/null 2>&1 && [ ! -e "$RULE" ] && CAN_BURN=1
CAN_BOOT=""; command -v systemctl >/dev/null 2>&1 && [ -d "$HOME/.config" ] && CAN_BOOT=1
MOUNT_UNIT=/etc/systemd/system/emupod-mount.service
CAN_MOUNT=""; command -v systemctl >/dev/null 2>&1 && command -v sudo >/dev/null 2>&1 && { [ -d /sys/module/nbd ] || modinfo nbd >/dev/null 2>&1; } && CAN_MOUNT=1
TOTAL=$((6 + ${CAN_BURN:-0} + ${CAN_MOUNT:-0} + ${CAN_BOOT:-0})); STEP=0
step() { STEP=$((STEP + 1)); say ""; say "$STEP of $TOTAL: $1"; }
say "This setup script will guide you through the $TOTAL steps needed to set up emupod on your computer."

step "Where to install"
ask "Install folder?" "$HOME_DIR"; HOME_DIR="$ANSWER"
mkdir -p "$HOME_DIR/bin"

# 2. the release: the program for this machine and its checksum, from the GitHub release
if [ -n "${EMUPOD_BASE_URL:-}" ]; then TAG="${EMUPOD_VERSION:-local}"            # for testing: a folder served over HTTP instead of the GitHub release
elif [ -n "${EMUPOD_VERSION:-}" ]; then TAG="$EMUPOD_VERSION"
else
    TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
    [ -n "$TAG" ] || die "could not find the latest release of $REPO (no network, or none published yet)."
fi
BASE="${EMUPOD_BASE_URL:-https://github.com/$REPO/releases/download/$TAG}"
SUMS="$(curl -fsSL "$BASE/SHA256SUMS")" || die "could not download $BASE/SHA256SUMS"
FILE="emupod-linux-$ARCH"
WANT="$(printf '%s\n' "$SUMS" | awk -v f="$FILE" '$2 == f {print $1}')"
[ -n "$WANT" ] || die "release $TAG has no program for $ARCH."
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
step "Downloading emupod"
note "downloading $FILE for $ARCH ($TAG)"
curl -fsSL -o "$TMP/$FILE" "$BASE/$FILE"
GOT="$(sha256sum "$TMP/$FILE" | awk '{print $1}')"
[ "$WANT" = "$GOT" ] || die "the download does not match its checksum (wanted $WANT, got $GOT); nothing was installed."

# 3. the program: one file, put in place whole (a running emupod keeps its old copy until it restarts)
chmod +x "$TMP/$FILE"
"$TMP/$FILE" --version >/dev/null 2>&1 || die "the downloaded program does not run on this machine."
mv -f "$TMP/$FILE" "$HOME_DIR/bin/emupod"
note "$("$HOME_DIR/bin/emupod" --version) installed in $HOME_DIR/bin"

# 4. where things live
step "Your cards"
ask "Folder for your iPod cards (card images; the built-in card, Rainbow Road, is put there at first start)?" "$HOME_DIR/cards"; CARDS="$ANSWER"
# the music folder: look for folders holding songs first (home, ~/Music, /media, /mnt, /srv; at most 15 s, so a big share cannot hang
# this), then offer them by number
step "Your music"
note "looking for music on this machine (you can change the folder later, on the Cards page)..."
FOUND="$( (timeout 15 find "$HOME" /media /mnt /srv -maxdepth 6 -type f \( -iname '*.mp3' -o -iname '*.m4a' -o -iname '*.flac' -o -iname '*.aac' \
          -o -iname '*.wav' -o -iname '*.aif' -o -iname '*.aiff' -o -iname '*.m4b' \) -not -path '*/.*' 2>/dev/null || true) \
    | awk -v home="$HOME" '{ p = $0; root = (index(p, home "/") == 1) ? home : "/" substr(p, 2, index(substr(p, 2), "/") - 1)
          rest = substr(p, length(root) + 2); n = split(rest, c, "/"); d = root; lim = (root == home) ? 1 : 2; for (i = 1; i < n && i <= lim; i++) d = d "/" c[i]; if (n == 1) d = root
          k[d]++ } END { for (d in k) print k[d] "\t" d }' | sort -rn | head -n 9)"
if [ -n "$FOUND" ]; then
    say "Music folders found on this machine:"
    printf '%s\n' "$FOUND" | awk -F'\t' '{ printf "  %d) %s  (%d song%s)\n", NR, $2, $1, ($1 == 1 ? "" : "s") }'
    ask "Music folder the iPod page may add songs from: a number, a path, or 0 for none?" "1"
    case "$ANSWER" in
        [0-9]) MUSIC="$(printf '%s\n' "$FOUND" | awk -F'\t' -v n="$ANSWER" 'NR == n { print $2 }')" ;;
        *) MUSIC="$ANSWER" ;;
    esac
else
    ask "No music found in the usual places. A music folder the iPod page may add songs from (Enter for none)?" ""; MUSIC="$ANSWER"
fi
[ -z "$MUSIC" ] || [ -d "$MUSIC" ] || { say "No such folder: $MUSIC (music folder left empty)"; MUSIC=""; }
# the port: a free one (an older emupod, or anything else, may hold the usual 8797; a running emupod from this folder is the one being
# replaced, so its port counts as free)
step "The web page"
PORT="${EMUPOD_PORT:-8797}"
OURS=""; command -v systemctl >/dev/null 2>&1 && systemctl --user is-active --quiet emupod.service 2>/dev/null && OURS=1
while :; do
    FREE="$PORT"; while port_busy "$FREE" && { [ -z "$OURS" ] || [ "$FREE" != "$PORT" ]; }; do FREE=$((FREE + 1)); done
    [ "$FREE" = "$PORT" ] || say "Port $PORT is already in use by another program (an older emupod?)."
    ask "Port for the web page?" "$FREE"; PORT="$ANSWER"
    if port_busy "$PORT" && [ -z "$OURS" ]; then say "Port $PORT is in use; choose another."; else break; fi
done
yesno "Listen on the whole network, so your phone can reach it? (No: only this machine.)" Y
case "$ANSWER" in Y) HOSTARG="0.0.0.0" ;; *) HOSTARG="127.0.0.1" ;; esac
mkdir -p "$CARDS"

# 4b. optional: burning cards. Writing a card in a reader needs write access to its device node (root's and the disk group's by default).
# A udev rule hands removable USB disks (card readers, flash drives; not internal disks, not USB hard drives) to the plugdev group.
# It is named 70-: the USB facts it tests are set by 60-persistent-storage.rules, which must run first.
if [ -n "$CAN_BURN" ]; then
    step "Burning flash cards (optional)"
    say "The emupod webapp can optionally be used to burn card images to physical CompactFlash (CF) or SecureDigital (SD) cards"
    say "that have been inserted into your computer. However, emupod needs escalated privileges to do this."
    if [ "${EMUPOD_YES:-}" = 1 ] || [ ! -r /dev/tty ]; then ANSWER=N          # no one to ask: nothing is done with sudo unattended
    else yesno "Grant those privileges now?" Y; fi
    case "$ANSWER" in Y)
        if printf '%s\n' 'SUBSYSTEM=="block", ENV{DEVTYPE}=="disk", ENV{ID_BUS}=="usb", ATTR{removable}=="1", GROUP="plugdev"' | sudo tee "$RULE" >/dev/null \
           && sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=block; then
            if id -nG "$USER" | tr ' ' '\n' | grep -qx plugdev; then note "card burning is allowed"
            else sudo usermod -aG plugdev "$USER" && say "Card burning is allowed after you log out and back in (you were added to the plugdev group)."; fi
        else say "Could not add the rule: card burning stays off (the rest of emupod works)."; fi ;;
    *) note "card burning stays off (to allow it later, run this installer again)" ;;
    esac
fi

# 4c. optional: mounting the emulated iPod's disk. emupod runs as this user; attaching a block device and mounting it need root, so a
# small root service (emupod mount-service, set up by `emupod setup-automount`) does only that, for this user. An installed one is
# updated to this version (it runs its own copy of the program).
if [ -n "$CAN_MOUNT" ]; then
    step "Mounting the iPod's disk (optional)"
    say "emupod runs as your own user, without root privileges. A few things need root, such as mounting the emulated iPod's disk"
    say "when you plug in its FireWire cable on the iPod page. For those, emupod can install a small service that runs as root and"
    say "does only that: it mounts the iPod's disk at /media/emupod when the cable goes in, and unmounts it before the cable comes out."
    say "Only your user can ask it to do anything."
    if [ "${EMUPOD_YES:-}" = 1 ] || [ ! -r /dev/tty ]; then ANSWER=N          # no one to ask: nothing is done with sudo unattended
    elif [ -e "$MOUNT_UNIT" ]; then yesno "The mount service is installed: update it to this version?" Y
    else yesno "Install the mount service now?" Y; fi
    case "$ANSWER" in Y)
        if sudo "$HOME_DIR/bin/emupod" setup-automount --user "$USER"; then note "the iPod's disk is mounted automatically"
        else say "Could not set up the mount service: disk mode still works, attached by hand (the iPod page shows how)."; fi ;;
    *) note "the mount service is $([ -e "$MOUNT_UNIT" ] && echo 'left as it is' || echo 'not installed') (later: sudo $HOME_DIR/bin/emupod setup-automount)" ;;
    esac
fi

# 5. a start script, and the optional system service
CMD="$HOME_DIR/bin/emupod web --host $HOSTARG --port $PORT --cards $CARDS"
[ -z "$MUSIC" ] || CMD="$CMD --music-root $MUSIC"
ENVLINE=""; [ -z "${EMUPOD_DEMO_URL:-}" ] || ENVLINE="EMUPOD_DEMO_URL=$EMUPOD_DEMO_URL"    # for testing: the built-in card from another site
printf '#!/bin/sh\n%bexec %s "$@"\n' "${ENVLINE:+export $ENVLINE\n}" "$CMD" > "$HOME_DIR/start.sh"; chmod +x "$HOME_DIR/start.sh"
if [ -n "$CAN_BOOT" ]; then
    step "Starting with this machine"
    yesno "Start emupod automatically when this machine boots (a systemd user service)?" Y
    case "$ANSWER" in Y)
        mkdir -p "$HOME/.config/systemd/user"
        printf '[Unit]\nDescription=emupod: an iPod in your browser\nAfter=network-online.target\n\n[Service]\n%bExecStart=%s\nRestart=on-failure\nTimeoutStopSec=600\n\n[Install]\nWantedBy=default.target\n' "${ENVLINE:+Environment=$ENVLINE\n}" "$CMD" > "$HOME/.config/systemd/user/emupod.service"
        systemctl --user daemon-reload && systemctl --user enable emupod.service && systemctl --user restart emupod.service && STARTED=1 || say "Could not enable the service: start emupod by hand with $HOME_DIR/start.sh"
        loginctl enable-linger "$USER" 2>/dev/null || say "For emupod to start at boot without you logged in, run: sudo loginctl enable-linger $USER" ;;
    esac
fi

IP="$(hostname -I 2>/dev/null | awk '{print $1}')"; [ -n "$IP" ] || IP="localhost"
step "Done"
note "emupod is installed$([ "${STARTED:-}" = 1 ] && echo ' and running')"
note "the first start downloads the built-in card, Rainbow Road (a few megabytes, one time)"
note "to uninstall: run sudo $HOME_DIR/bin/emupod remove-automount if the mount service is installed, then remove $HOME_DIR and $HOME/.config/systemd/user/emupod.service (and $RULE, with sudo, if card burning was allowed); your cards stay in $CARDS"
say ""
[ "${STARTED:-}" = 1 ] || say "Start it:   $HOME_DIR/start.sh"
say "Open on your phone (same Wi-Fi):   http://$IP:$PORT/"
