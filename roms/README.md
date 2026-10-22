# Firmware images

emupod ships with **no Apple firmware** — you supply it. Drop the iPod firmware
`.ipsw` files here:

| File              | Device                              | Used by         |
|-------------------|-------------------------------------|-----------------|
| `iPod_1.1.5.ipsw` | iPod 1st/2nd gen (PP5002, Family 1) | `emupod boot gen1` |
| `iPod_2.2.3.ipsw` | iPod 3rd gen (PP5002, Family 2)     | `emupod boot gen3` (WIP) |

These are Apple's plaintext PortalPlayer firmware images (a ZIP containing a
`Firmware-*` blob). They are copyrighted by Apple and are **gitignored** — never
commit them.

Once a file is here, run `emupod setup gen1` (or just `emupod boot gen1`, which
sets up automatically).
