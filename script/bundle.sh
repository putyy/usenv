#!/bin/sh
set -eu

make_dmg=false
case "$#:$*" in
    0:) profile=debug ;;
    1:--release) profile=release ;;
    1:--dmg) profile=release; make_dmg=true; set -- --release ;;
    *) printf 'Usage: sh script/bundle.sh [--release | --dmg]\n' >&2; exit 2 ;;
esac

cd "$(dirname "$0")/.."
root=$(pwd -P)
cargo build --locked --target-dir "$root/target" "$@"
package=$(cargo pkgid --locked)
version=${package##*#}
version=${version##*@}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/usenv-bundle.XXXXXXXX")
trap 'rm -rf "$scratch"' EXIT
trap 'exit 1' HUP INT TERM

# APFS can preserve the old spelling when overwriting a case-only rename.
rename_legacy_output() {
    old_path="$root/dist/$1"
    new_path="$root/dist/$2"
    if [ -n "$(/usr/bin/find "$root/dist" -maxdepth 1 -name "$1" -print)" ]; then
        if [ "$old_path" -ef "$new_path" ]; then
            mv "$old_path" "$scratch/$1"
            mv "$scratch/$1" "$new_path"
        elif [ ! -e "$new_path" ]; then
            mv "$old_path" "$new_path"
        fi
    fi
}

iconset="$scratch/usenv.iconset"
mkdir "$iconset"
for dimension in 16 32 128 256 512; do
    retina=$((dimension * 2))
    /usr/bin/sips -Z "$dimension" "$root/build/usenv-logo.png" \
        --out "$iconset/icon_${dimension}x${dimension}.png" >/dev/null
    /usr/bin/sips -Z "$retina" "$root/build/usenv-logo.png" \
        --out "$iconset/icon_${dimension}x${dimension}@2x.png" >/dev/null
done
/usr/bin/iconutil -c icns -o "$scratch/usenv.icns" "$iconset"

mkdir -p "$root/dist"
rename_legacy_output usenv.app Usenv.app
app="$root/dist/Usenv.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$root/target/$profile/usenv" "$app/Contents/MacOS/usenv"
install -m 644 "$scratch/usenv.icns" "$app/Contents/Resources/usenv.icns"
plist="$app/Contents/Info.plist"
install -m 644 "$root/build/macos/Info.plist" "$plist"
/usr/bin/plutil -insert CFBundleShortVersionString -string "$version" "$plist"
/usr/bin/plutil -insert CFBundleVersion -string "$version" "$plist"
/usr/bin/codesign --force --sign - "$app"

if [ "$make_dmg" = true ]; then
    architecture=$(/usr/bin/lipo -archs "$app/Contents/MacOS/usenv")
    case "$architecture" in
        'arm64 x86_64'|'x86_64 arm64') architecture=universal ;;
    esac
    mkdir "$scratch/stage"
    /usr/bin/ditto "$app" "$scratch/stage/Usenv.app"
    ln -s /Applications "$scratch/stage/Applications"
    /usr/bin/hdiutil create -fs HFS+ -format UDZO -volname Usenv \
        -srcfolder "$scratch/stage" "$scratch/Usenv.dmg"
    /usr/bin/hdiutil verify "$scratch/Usenv.dmg"
    rename_legacy_output "usenv-dev-macos-$architecture.dmg" "Usenv-dev-macos-$architecture.dmg"
    dmg="$root/dist/Usenv-dev-macos-$architecture.dmg"
    mv -f "$scratch/Usenv.dmg" "$dmg"
    printf '%s\n' "$dmg"
else
    printf '%s\n' "$app"
fi
