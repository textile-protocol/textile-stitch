# dmgbuild settings for Stitch.dmg.
# Invoked by make-dmg.sh; paths come in via -D app=… -D background=….
# Writes .DS_Store directly — no Finder/AppleScript — so CI gets the
# drag-to-Applications background every time.
import os.path

application = defines["app"]  # noqa: F821 — injected by dmgbuild
appname = os.path.basename(application.rstrip("/"))
background = defines["background"]  # noqa: F821

format = "UDZO"
compression_level = 9

files = [application]
symlinks = {"Applications": "/Applications"}

# Match dmg-background.png (600x400) and the old AppleScript layout.
window_rect = ((200, 120), (600, 400))
icon_size = 120
icon_locations = {
    appname: (150, 205),
    "Applications": (455, 205),
}

show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
default_view = "icon-view"
include_icon_view_settings = True
arrange_by = None
