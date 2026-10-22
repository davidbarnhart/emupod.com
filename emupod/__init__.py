"""emupod -- a software emulator of the 1st/2nd- and 3rd-generation Apple iPod (PortalPlayer PP5002).

Goal: run the real iPod firmware, see its display, and hear its audio -- starting as a tool for
firmware exploration and growing toward a faithful recreation of the device in software.

The proven gen-1 boot/LCD/input emulator lives under ``emupod.core`` (ported from the openpod
reverse-engineering project, where it boots Apple's unmodified osos to the first-run UI). The
top-level package wraps it with project structure: artifact setup, a CLI, and a console display.
"""
__version__ = "0.1.0"
