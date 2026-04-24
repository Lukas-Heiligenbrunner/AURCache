#!/usr/bin/env bash
# this script takes two arguments and sets up unattended AUR access for user ${1} via a helper, ${2}
set -o pipefail
set -o errexit
set -o nounset
set -o verbose
set -o xtrace

AUR_USER="${1:-ab}"

# overwrite config if amd64 imaage
if [ "$TARGETPLATFORM" = "linux/amd64" ]; then
  cp /etc/pacman.conf.amd64 /etc/pacman.conf
fi

# fix landlock errors in container builds
sed -i '/^\[options\]/a DisableSandbox' /etc/pacman.conf

# we're gonna need sudo to use the helper properly
pacman -Syyu --noconfirm
pacman --sync --needed --noconfirm --noprogressbar pacman-contrib

# repopulate keychain
pacman-key --init

if [ -f /usr/share/pacman/keyrings/archlinuxarm.gpg ]; then
    pacman-key --populate archlinuxarm
else
    pacman-key --populate archlinux
fi

cp /etc/pacman.d/mirrorlist /etc/pacman.d/mirrorlist.backup
# uncomment all mirrors
sed -i 's/^#Server/Server/' /etc/pacman.d/mirrorlist.backup
# test speed of mirrors and select fastest ones
rankmirrors -n 10 /etc/pacman.d/mirrorlist.backup > /etc/pacman.d/mirrorlist
rm /etc/pacman.d/mirrorlist.backup

pacman --sync --needed --noconfirm --noprogressbar sudo base-devel git rust || echo "Nothing to do"
git config --global --add safe.directory '*'

# create the user
AUR_USER_HOME="/var/${AUR_USER}"
useradd "${AUR_USER}" --system --shell /usr/bin/nologin --create-home --home-dir "${AUR_USER_HOME}"

# lock out the AUR_USER's password
passwd --lock "${AUR_USER}"

# give the aur user passwordless sudo powers for pacman
echo "${AUR_USER} ALL=(ALL) NOPASSWD: /usr/bin/pacman" > "/etc/sudoers.d/allow_${AUR_USER}_to_pacman"
echo "${AUR_USER} ALL=(ALL) NOPASSWD: /usr/bin/pacman-key" >> "/etc/sudoers.d/allow_${AUR_USER}_to_pacman"
echo "${AUR_USER} ALL=(ALL) NOPASSWD: /usr/bin/chmod" >> "/etc/sudoers.d/allow_${AUR_USER}_to_pacman"

# let root cd with sudo
echo "root ALL=(ALL) CWD=* ALL" > /etc/sudoers.d/permissive_root_Chdir_Spec

# build config setup
sudo -u ${AUR_USER} -D~ bash -c 'mkdir -p .config/pacman'

# use all possible cores for builds
sudo -u ${AUR_USER} -D~ bash -c 'echo MAKEFLAGS="-j\$(nproc)" > .config/pacman/makepkg.conf'

# don't compress the packages built here
#sudo -u ${AUR_USER} -D~ bash -c 'echo PKGEXT=".pkg.tar" >> .config/pacman/makepkg.conf'

# setup storage for AUR packages built
NEW_PKGDEST="/build"
NPDP=$(dirname "${NEW_PKGDEST}")
mkdir -p "${NPDP}"
install -o "${AUR_USER}" -d "${NEW_PKGDEST}"
sudo -u ${AUR_USER} -D~ bash -c "echo \"PKGDEST=${NEW_PKGDEST}\" >> .config/pacman/makepkg.conf"

# setup place for foreign packages
FOREIGN_PKG="/var/cache/foreign-pkg"
FPP=$(dirname "${FOREIGN_PKG}")
mkdir -p "${FPP}"
install -o "${AUR_USER}" -d "${FOREIGN_PKG}"

# Prepare paru helper.
# Currently builds from source, from a fork with some fixes.
# Eventually, we could build from upstream, from crates.io, or even install the paru package itself.
sudo -u "${AUR_USER}" bash -c "cargo install --git https://github.com/gyscos/paru"
cp "${AUR_USER_HOME}/.cargo/bin/paru" /usr/local/bin/
chmod 755 /usr/local/bin/paru

# Remove all pacman caches
pacman -Scc --noconfirm || echo "Pacman cache already clean"

# Remove orphaned packages (installed as dependencies but no longer needed)
pacman -Rns --noconfirm $(pacman -Qtdq) || echo "No orphaned packages to remove"

# remove previously installed packages
pacman -Rns --noconfirm rust || echo "Build dependencies already removed"

# cleanup
sudo rm -rf "${NEW_PKGDEST}"/*
rm -rf "${AUR_USER_HOME}/.cache/go-build"
rm -rf "${AUR_USER_HOME}/.cargo"
rm -rf /tmp/*
rm -rf /root/.cargo /usr/share/cargo || true
rm -rf /var/tmp/* /var/cache/* || true

echo "Cleanup complete"
