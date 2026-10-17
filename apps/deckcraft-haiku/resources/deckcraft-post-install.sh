#!/bin/sh
# DeckCraft's post-install script (boot/post-install/deckcraft.sh in the package).
#
# Registers the application with the MIME database, whose packaged record names the machine
# the package was built on, and makes DeckCraft the preferred application of the presentation
# types that have none, so a double-click in Tracker opens them before DeckCraft has ever run
# (it does the same at each start; the types are services.rs's DOCUMENT_TYPES).
# A preferred application someone already chose stays.
signature=application/x-vnd.storytold-DeckCraft
app=$(findpaths -p "$0" B_FIND_PATH_APPS_DIRECTORY DeckCraft 2>/dev/null)
[ -n "$app" ] && [ -e "$app" ] && mimeset -a -f "$app"
setmime -set application/x-deckcraft -short "DeckCraft presentation" -extension deckcraft
for type in \
        application/x-deckcraft \
        application/vnd.openxmlformats-officedocument.presentationml.presentation \
        application/vnd.openxmlformats-officedocument.presentationml.template \
        application/vnd.openxmlformats-officedocument.presentationml.slideshow; do
	setmime -dump "$type" 2>/dev/null | grep -q -- -preferredAppSig \
		|| setmime -set "$type" -preferredAppSig "$signature"
done
exit 0
