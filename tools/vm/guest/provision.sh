#!/usr/bin/env bash
# Run once by cloud-init, as root, on the first boot of the base image: turns
# the server cloud image into the GNOME desktop Okapi is tested on. The
# accounts already exist (cloud-init made them). Arguments: the account logged
# in automatically, the other human account, the line that says it all went.
set -euxo pipefail

readonly admin=$1
readonly other=$2
readonly marker=$3
export DEBIAN_FRONTEND=noninteractive

# Runs would otherwise race these against the tests (a locked dpkg, an APT
# cache filled or emptied behind the Cleaner's back).
systemctl disable --now apt-daily.timer apt-daily-upgrade.timer unattended-upgrades.service

apt-get update
# Without recommends: a GNOME session and what the tests drive, not the
# whole Ubuntu desktop (office suite, Firefox snap...).
apt-get install --yes --no-install-recommends \
    ubuntu-desktop-minimal \
    gnome-terminal \
    language-pack-fr language-pack-gnome-fr \
    at-spi2-core python3-gi gir1.2-atspi-2.0 \
    orca \
    packagekit \
    flatpak \
    docker.io \
    wl-clipboard \
    libpam-tmpdir \
    auditd \
    bindfs \
    dosfstools \
    dpkg-dev apt-utils \
    gnupg \
    nfs-kernel-server
apt-get clean

# pam_tmpdir moves every session's TMPDIR: off by default, a scenario turns it
# on (`pam-auth-update --enable tmpdir`) and restarts the session.
pam-auth-update --package --remove tmpdir

# A Flathub application with its runtime, and a second runtime nothing uses.
flatpak remote-add --system --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install --system --noninteractive flathub io.mpv.Mpv
used=$(flatpak info --system --show-runtime io.mpv.Mpv)
# Refs listed carry their kind; older branches only with --all.
orphan=$(flatpak remote-ls --system --all --runtime --columns=ref flathub \
    | grep "^runtime/org.freedesktop.Platform/x86_64/" | grep -v "^runtime/$used\$" | sort -V | tail -n 1)
flatpak install --system --noninteractive flathub "$orphan"

# Only once gdm3 is installed: written before, it is a changed conffile and
# dpkg stops on its question.
cat > /etc/gdm3/custom.conf <<EOF
[daemon]
AutomaticLoginEnable=true
AutomaticLogin=$admin
EOF

dconf update
systemctl set-default graphical.target

# No first-run assistant.
for account in "$admin" "$other"; do
    install -d --owner="$account" --group="$account" "/home/$account/.config"
    install --owner="$account" --group="$account" --mode=0644 /dev/null \
        "/home/$account/.config/gnome-initial-setup-done"
done

# The base is provisioned once; the runs boot overlays without a seed.
touch /etc/cloud/cloud-init.disabled
echo "$marker" > /dev/ttyS0
