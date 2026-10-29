#!/usr/bin/env bash
# Cross-build and package entirely on Linux with Haiku's host tools.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$ROOT"
. tools/haiku/env.sh
TOOLS=${HAIKU_HOST_TOOLS:-/mnt/HaikuWork/build/fw13-x86_64/objects/linux/x86_64/release/tools}
RES="$ROOT/apps/deckcraft-haiku/resources"
OUT="$ROOT/artifacts/haiku"
VERSION=$(awk '$1 == "version" { print $2; exit }' "$RES/DeckCraft.PackageInfo")
if [[ ${1:-} != --no-build ]]; then bash tools/haiku/build.sh; fi
mkdir -p "$OUT"
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/deckcraft-pkg-XXXXXX")
trap 'rm -rf -- "$STAGE"' EXIT
DOCS="$STAGE/documentation/packages/deckcraft"
mkdir -p "$STAGE/apps" "$STAGE/data/deskbar/menu/Applications" "$STAGE/boot/post-install" "$DOCS"
"$HAIKU_CROSS-strip" -o "$STAGE/apps/DeckCraft" "$CARGO_TARGET_DIR/x86_64-unknown-haiku/release/DeckCraft"
(cd "$RES" && "$TOOLS/rc/rc" -o "$STAGE/DeckCraft.rsrc" DeckCraft.rdef)
"$TOOLS/xres" -o "$STAGE/apps/DeckCraft" "$STAGE/DeckCraft.rsrc"
"$TOOLS/resattr/resattr" -O -o "$STAGE/apps/DeckCraft" "$STAGE/DeckCraft.rsrc"
rm "$STAGE/DeckCraft.rsrc"
cp "$RES/DeckCraft.PackageInfo" "$STAGE/.PackageInfo"
cp "$RES/deckcraft-post-install.sh" "$STAGE/boot/post-install/deckcraft.sh"
chmod 755 "$STAGE/boot/post-install/deckcraft.sh" "$STAGE/apps/DeckCraft"
cp README.md LICENSE-MIT LICENSE-APACHE NOTICE docs/haiku.md "$DOCS/"
# egui's default fonts come from the storytold/egui commit patched in Cargo.toml (a git checkout).
mkdir -p "$DOCS/licenses/epaint_default_fonts"
EGUI_REV=$(sed -n 's/^epaint_default_fonts = .*rev = "\([0-9a-f]*\)".*/\1/p' Cargo.toml)
cp "$CARGO_HOME"/git/checkouts/egui-*/"${EGUI_REV:0:7}"/crates/epaint_default_fonts/fonts/*.txt "$DOCS/licenses/epaint_default_fonts/"
# hayro, the PDF renderer the print backend uses, embeds fonts, CMaps and colour profiles.
for crate in hayro-cmap hayro-interpret hayro-jpeg2000; do
	version=$(awk -F '"' -v name="$crate" '$0 == "name = \"" name "\"" { found = 1; next }
		found && /^version = / { print $2; exit }' Cargo.lock)
	mkdir -p "$DOCS/licenses/$crate"
	cp "$CARGO_HOME"/registry/src/*/"$crate-$version"/assets/*LICENSE* "$DOCS/licenses/$crate/"
done
ln -s ../../../../apps/DeckCraft "$STAGE/data/deskbar/menu/Applications/DeckCraft"
"$TOOLS/package/package" create -C "$STAGE" "$OUT/deckcraft-$VERSION-x86_64.hpkg"
