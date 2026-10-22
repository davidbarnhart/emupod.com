#!/usr/bin/env python3
"""The card store's stock card (emupod/cards.py; David, 2026-10-16), on scratch stores, through flashpod's library.

    python3 tests/test_cards.py [--flashpod DIR]      (default ~/source/flashpod-main; skipped if missing)

1. An EMPTY store gets exactly one card, "stock": 10 GB, Apple's 1.5 (osos 98251add), a valid iTunesDB, 0 tracks.
2. Starting again does not make a second one, and leaves the store byte-unchanged.
3. A store holding any card is left byte-unchanged: no stock card.
4. An interrupted build's hidden leftover is not a card and does not stop the stock card being made.
"""
import hashlib, os, shutil, sys, tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
FP = sys.argv[sys.argv.index("--flashpod") + 1] if "--flashpod" in sys.argv else os.path.expanduser("~/source/flashpod-main")


def state(d):
    """Every file in the store with its size and content hash (sparse images hashed in full)."""
    out = {}
    for f in sorted(os.listdir(d)):
        p = os.path.join(d, f)
        if os.path.isfile(p):
            h = hashlib.sha256()
            with open(p, "rb") as fh:
                for b in iter(lambda: fh.read(1 << 22), b""): h.update(b)
            out[f] = (os.path.getsize(p), h.hexdigest())
    return out


def main():
    if not os.path.isdir(os.path.join(FP, "flashpod")): print("SKIP no flashpod at %s" % FP); return 0
    from emupod.cards import Cards, CardError   # noqa: F401  (web.py imports CardError; 615d225 dropped it unnoticed)
    bad = 0
    def check(ok, what):
        nonlocal bad; bad += not ok; print(("ok    " if ok else "FAIL  ") + what)
    base = tempfile.mkdtemp(prefix="cards-test-", dir=os.environ.get("EMUPOD_TEST_TMP"))
    try:
        empty = os.path.join(base, "empty"); os.makedirs(empty)
        c = Cards(empty, FP)
        op = c.ensure_stock(wait=True)
        names = c.card_names()
        info = c.fp.inspect(os.path.join(empty, "stock.img")).to_dict() if names == ["stock"] else {}
        f, db = info.get("firmware") or {}, info.get("itunesdb") or {}
        check(op is not None and op["state"] == "done" and names == ["stock"], "an empty store gets one card, stock (%s; %s)" % (names, op and op.get("error")))
        check(os.path.getsize(os.path.join(empty, "stock.img")) == 10 * 10 ** 9 if names else False, "stock is 10 GB")
        check((f.get("osos") or {}).get("sha256", "").startswith("98251add") and f.get("build") == "stock 1.5",
              "stock carries Apple's 1.5 (build %r)" % f.get("build"))
        check(db.get("state") == "ok" and db.get("tracks") == 0, "stock has a valid, empty iTunesDB (%s)" % db)
        before = state(empty)
        check(Cards(empty, FP).ensure_stock(wait=True) is None and state(empty) == before, "a second start makes no second card; the store is unchanged")

        other = os.path.join(base, "other"); os.makedirs(other)
        with open(os.path.join(other, "mine.img"), "wb") as fh: fh.truncate(1 << 20); fh.write(b"x")
        before = state(other)
        check(Cards(other, FP).ensure_stock(wait=True) is None and state(other) == before, "a store with a card is left byte-unchanged")

        left = os.path.join(base, "leftover"); os.makedirs(left)
        with open(os.path.join(left, ".stock.building.img"), "wb") as fh: fh.write(b"half")
        c = Cards(left, FP)
        check(c.card_names() == [], "an interrupted build's leftover is not a card")
        op = c.ensure_stock(wait=True)
        check(op and op["state"] == "done" and c.card_names() == ["stock"] and not os.path.exists(os.path.join(left, ".stock.building.img")),
              "the stock card is still made, and the leftover gone")
    finally:
        shutil.rmtree(base, ignore_errors=True)        # this test's own scratch stores
    print("all checks pass" if not bad else "%d check(s) failed" % bad)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
