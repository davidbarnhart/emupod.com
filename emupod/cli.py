"""emupod command-line interface."""
import argparse, os, sys
from . import paths, __version__


def cmd_web(args):
    from . import web
    if not paths.GENS[args.gen]["bootable"]:
        sys.exit("%s boot is not yet wired up. See docs/ROADMAP.md." % args.gen)
    web.main(args)


def main(argv=None):
    ap = argparse.ArgumentParser(prog="emupod", description="a software emulator of the 1st/2nd-generation Apple iPod, served to a browser")
    ap.add_argument("--version", action="version", version="emupod " + __version__)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("web", help="serve the iPod front panel to a browser, recording every input and screen")
    p.add_argument("gen", nargs="?", default="gen1", choices=list(paths.GENS))
    p.add_argument("--host", help="address to listen on (default 127.0.0.1; 0.0.0.0 serves the local network)")
    p.add_argument("--port", type=int, default=8797)
    p.add_argument("--notice", default="", help="a message the page shows when it opens (the web app sets it when "
                   "it restarts itself, e.g. after a card fails to boot)")
    p.add_argument("--cards", default=os.path.expanduser("~/emupod-cards"), help="the card store's directory (card images)")
    p.add_argument("--flashpod", default=os.environ.get("EMUPOD_FLASHPOD", ""),
                   help="a flashpod checkout whose library manages cards (default: the installed flashpod)")
    p.add_argument("--music-root", help="a folder of music the cards page may add from")
    p.add_argument("--firmware-dir", action="append", help="a folder of firmware files (.ipsw...) the page may flash")
    p.add_argument("--card", help="boot this card from the store (its own firmware) instead of the standard disk")
    p.add_argument("--discard", action="store_true", help="with --card: keep the iPod's writes in memory only")
    p.add_argument("--no-remember", action="store_true", help="(the web app sets this) do not record this boot as the card the next start boots")
    p.add_argument("--then-op", help="(the web app sets this when it restarts itself) an operation to run on a card "
                   "once the iPod is up without it, as JSON {card, op, args}; the iPod goes back onto the card if it succeeds")
    p.add_argument("--jack", action="store_true", help="start with the audio on the host: the sound plays on "
                   "this machine's output, not in the browser (the page's audio switch)")
    p.add_argument("--audio-cmd", help="the program the host's sound is piped to, raw s16le stereo 44.1 kHz on "
                   "stdin (default: aplay -q -t raw -f S16_LE -r 44100 -c 2 -B 500000)")
    p.set_defaults(func=cmd_web)

    args = ap.parse_args(argv)
    args.func(args)


if __name__ == "__main__":
    main()
