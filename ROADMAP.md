# PartShield roadmap

The goal: a backup should take four clicks and one progress bar, with
everything else in an advanced mode.

## Before calling it 1.0

1. ~~Own repository with CI, README and license.~~ Done: `KorVeix/PartShield`,
   GPL-3.0.
2. A real backup of an everyday disk (Btrfs, ~653 GiB), restored and verified
   end to end on real hardware.

## Done

- Home screen with "Back up" and "Restore" (Restore still pending), an
  "Advanced" switch and a "Tools" menu.
- Simple backup: whole disk or partitions with checkboxes, destination on
  another disk, one progress bar, and a green, amber or red result.
- Whole disk: partition table (`sfdisk --dump`) plus each partition; Btrfs
  via send, everything else sector by sector.
- "Structure" step for ext2/3/4, vfat, exFAT, XFS and NTFS using their
  read-only fsck, so backups of those partitions can come out valid.
- Unclean NTFS (Windows hibernated or shut down with Fast Startup) and
  BitLocker are detected: a warning before copying, and the backup is never
  green.
- A whole-disk raw image is verified partition by partition (fsck and mount).
- The space estimate counts the scratch space verification needs, and
  measures the destination even when it isn't mounted.

## v0.2

Scope of 0.1 is frozen. Everything below is planned and not started in
`master`, unless noted.

1. **Restore from the GUI.** A backup set (`<disk>-<date>.set.json`: disk
   model and serial, partition table, one report per partition), an engine
   that writes the table and then each partition (raw as is; Btrfs with
   `mkfs`, receive and the original UUID), and the screens: pick a backup,
   pick the disk, confirm by typing the end of its serial number. Drafted in
   the branch `claude/restaurar-gui`: the engine passes an end-to-end test
   on loop devices; the screens are untested.
2. **Used-block images** for NTFS, ext4 and the rest, instead of copying free
   space.
3. **LUKS unlocking** from the GUI, so the filesystem inside is backed up and
   verified.
4. **Real progress**: percentage by bytes and time left.
5. **GParted and Xwayland on the ISO**, with root permissions inside cage.
6. **English interface** (gettext), Spanish kept as a translation.
7. **External drives mounted read-only at boot**, with existing backups
   detected.
8. **Advanced mode extras**: include snapshots, exclusions, sample size,
   signing keys, live log, re-verify an old backup.
9. Smaller items noted along the way:
   - Unclosed NTFS: consider unchecking it by default, not only warning.
   - Restoring a single raw report from the GUI, and onto a larger disk.
   - Recreating a restored Btrfs with the original feature flags, and nested
     subvolumes with their original paths.
   - The space estimate treats sector-by-sector scratch as full size, though
     it is written sparse.
   - vfat and exFAT structure checks are not exercised in CI (the container
     kernel can't mount them).

The sections below are the design notes for those items.

## Simple mode

The default: the live USB always starts in simple mode.

**Home screen:** two buttons, "Back up" and "Restore".

**Back up**, Acronis style:

1. **Source:** disk tree. The whole disk is selected by default; single
   partitions can be picked too.
2. **Destination:** external disks only, with a fits / doesn't fit warning.
3. **Progress:** one bar covering copy and verification, with percentage
   and time left.
4. **Result:**
   - 🟢 Verified: safe to restore.
   - 🟡 Backed up, but not fully verified.
   - 🔴 This backup is not valid.

**Restore:** pick the backup (newest first), pick the disk, start.

Anything that erases a disk asks for confirmation by model and serial
number, in simple mode too.

What simple mode decides on its own:

- All subvolumes, no snapshots.
- Send or raw depending on what's on the disk.
- Full verification, always; it can't be skipped.
- Signed report saved next to the backup.

## Advanced mode

Turned on with a switch at the top. It includes:

- What exists today: subvolume selection, method, and the detail of the five
  verification steps.
- Extras:
  - include snapshots
  - exclusions
  - sample size
  - signing keys
  - live log
  - re-verify an old backup
  - unlock LUKS

## External drives

- All of them are mounted **read-only** at boot, and existing backups on
  them are offered under "Restore".
- Only the chosen destination becomes read-write, and only while copying.
- A mounted source is unmounted automatically before copying.

## Tools

A separate button for utilities:

- **GParted**, with Xwayland on the ISO. Root permissions inside cage still
  need solving: launch it as root, or add a polkit agent.
- Later: a terminal and disk SMART status.

PartShield doesn't format disks. If the destination isn't usable, prepare
it first with GParted.

## Interface language

- English interface (gettext), chosen from the system language or at boot,
  with Spanish kept as a translation.

## Technical work

- **Whole-disk backup:** partition table plus each partition.
  - Btrfs, as today.
  - NTFS, EFI and the rest as an image of used blocks only (today the whole
    partition is copied, free space included).
  - Each partition is verified by restoring it.
- **Progress bar** with a real percentage. Today it advances per part (copy
  and verification); on a large sector-by-sector image it sits still until
  the copy finishes.
- **Restore from the GUI.**
- GParted and Xwayland on the ISO (the button already exists and turns on
  when GParted is installed).
