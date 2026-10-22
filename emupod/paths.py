"""Central path + machine-generation registry for emupod."""
import os

PKG  = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(PKG)                       # the emupod/ project root
CORE = os.path.join(PKG, "core")
ROMS = os.path.join(ROOT, "roms")                 # user-supplied firmware .ipsw (gitignored)
# generated artifacts (recordings, the audio choice): in a source checkout, work/ beside the code (gitignored); installed from a wheel, the
# package directory is not the place for them, so they go to the user's data directory (EMUPOD_WORK overrides both)
WORK = os.environ.get("EMUPOD_WORK") or (os.path.join(ROOT, "work") if os.path.isdir(os.path.join(ROOT, "rust"))
                                         else os.path.join(os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share"), "emupod"))

# Supported iPod generations. Both are PortalPlayer PP5002, FireWire, plaintext firmware.
GENS = {
    "gen1": {"ipsw": "iPod_1.1.5.ipsw", "name": "1st/2nd gen (PP5002, Family 1)", "bootable": True},
    "gen3": {"ipsw": "iPod_2.2.3.ipsw", "name": "3rd gen (PP5002, Family 2)",     "bootable": False},
}

def ipsw_path(gen):
    return os.path.join(ROMS, GENS[gen]["ipsw"])

def work_paths(gen):
    d = os.path.join(WORK, gen)
    return {
        "dir":  d,
        "osos": os.path.join(d, "osos.bin"),
        "aupd": os.path.join(d, "aupd.bin"),
        "fat":  os.path.join(d, "data_fat.img"),   # the old empty data volume (--legacy boots)
        "disk": os.path.join(d, "emu-disk.img"),   # whole-card image with music (core/make_emu_disk.py)
    }
