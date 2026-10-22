"""The card pages, served by emupod/web.py (API: /api/cards..., emupod/cards.py):

  /cards         the card library: each card drawn as an SD card (its size printed on it), its name and firmware
                 below; the card the iPod boots has a bold green box; a tap opens the card's setup
  /cards/NAME    card setup: breadcrumb iPod > Cards > NAME; boot it, firmware, the music library, the rest

One page; the script picks the screen from the URL."""

import os

CARDS_PAGE = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "pages", "cards.html"), encoding="utf-8").read()   # shared with the Rust server
