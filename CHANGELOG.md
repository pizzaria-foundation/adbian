# Changelog

## v0.2.0 — 2026-08-26

A remote shell for Symbian S60, over Bluetooth RFCOMM.

- **`rshelld`** is the agent: headless, no window group, invisible to the task list.
- **`rshell`** is the window that shows what it is doing.
- From a computer: list, read and write files, run commands, and push packages for
  installation.

**Install: this is two packages, and you want both.** Download `rshelld.sisx` and
`rshell.sis` and open each on the device. This is the one that cannot be installed
through ADBian, because ADBian is what is being installed.
