# Throwback

A CLI for working with Epilogue's GB Operator and SN Operator.

It dumps ROMs, backs up and restores saves, reads and sets the clock on RTC carts (such as Pokémon), pulls photos off a Game Boy Camera and writes new ones, flashes ROMs to flash carts, applies IPS, UPS, and BPS patches, and updates games to their latest version.

Throwback is an independent project. It talks to the Operator over USB and is not affiliated with Epilogue.

## What it works with

- **GB Operator**: Game Boy, Game Boy Color, Game Boy Advance
- **SN Operator**: Super Nintendo / Super Famicom

Plug the Operator into USB and insert a cartridge, throwback finds the device on its own.

## Install

macOS and Linux:

```
curl -fsSL https://raw.githubusercontent.com/nathankellenicki/throwback/main/scripts/install.sh | sh
```

Windows (PowerShell):

```
irm https://raw.githubusercontent.com/nathankellenicki/throwback/main/scripts/install.ps1 | iex
```

This puts Throwback in `~/.throwback/` and on your PATH. Run `throwback` to start. Or download a build from the [latest release](https://github.com/nathankellenicki/throwback/releases), unpack it, and run the binary directly.

## Building

Throwback is written in Rust. Rust 1.85 or newer is required.

```
cargo build --release
```

The binary is at `target/release/throwback`. Copy it onto your PATH to run it as `throwback`.

## Commands

Run `throwback` with no arguments to list the commands, or `throwback <command> --help` for the options on one of them.

Some commands (`write-photo`, `apply-patch`, `upgrade`) read something in and write something out. `--from` sets the source and `-o` sets the destination. Leave either off and that end is the cartridge. With neither, the command reads the cart and writes back to it. Writing to the cartridge asks you to confirm first. Pass `-y` to skip.

### info

Show what's in the slot without dumping anything: title, type, ROM and save size, region, and whether the cart is writeable.

```
throwback info
```

```
Type:            GBC
Title:           PM_CRYSTAL
Game ID:         P26D218
MBC:             MBC3+Timer+RAM+Battery (0x10)
ROM Size:        2 MB
Save:            32 KB
RTC:             Present
Region:          Non-Japan (International)
Version:         1
Header Checksum: 0x26 (valid)
Writeable:       No (retail/mask ROM)
```

Pass `--raw` to also print the raw signature bytes.

### dump-rom

Read the cartridge ROM to a file.

```
throwback dump-rom zelda.gbc
throwback dump-rom mario.sfc
```

GBA carts are trimmed to their real size. So are SNES carts that read back larger than the game (a 2.5 MB game the hardware reads as 4 MB, for example).

### read-save and write-save

Copy the cartridge's save to a file, and write it back later.

```
throwback read-save crystal.sav
throwback write-save crystal.sav
```

A save backed up here works in an emulator, and a save from an emulator works here. `write-save` replaces the save on the cart. It asks you to confirm first. Pass `-y` to skip, and keep the original somewhere safe.

For a cart with a clock (the MBC3 games like Pokémon Gold, Silver, and Crystal), `read-save` stores the clock in the save file, and `write-save` sets it back on the cart. This matches how emulators handle these saves. Pass `--no-rtc` to leave the clock out of a backup, or to restore the save without changing the cart's clock.

### read-rtc and write-rtc

Read or set the clock on a Game Boy cart that has one (the MBC3 games, such as Pokémon Gold, Silver, and Crystal).

```
throwback read-rtc
throwback read-rtc --output crystal.rtc
```

```
RTC: 47 days, 12:30:05
```

Set it by hand, or restore from a backup:

```
throwback write-rtc --days 5 --hours 12 --minutes 30 --seconds 0
throwback write-rtc --from crystal.rtc
```

If the cart's clock battery has died, the values read back out of range and Throwback says so.

### read-photos

Pull the photos off a Game Boy Camera. Each photo is saved as a PNG in the directory you name.

```
throwback read-photos photos/
```

The photos are small, so each one is scaled up 10x by default, with no smoothing. Pass `--scaling` to change the factor:

```
throwback read-photos photos/ --scaling 1x
```

Add `--framed` to include the border around each photo (the "Nintendo / Game Boy" frame, or the Hello Kitty borders on that cartridge):

```
throwback read-photos photos/ --framed
```

To read from a save file instead of the cart, use `--from`. With `--framed` you also point it at a camera ROM:

```
throwback read-photos photos/ --from camera.sav
throwback read-photos photos/ --from camera.sav --framed --rom camera.gb
```

### write-photo

Add a photo to a Game Boy Camera. Give it a PNG, with the camera in the Operator:

```
throwback write-photo test.png
```

Throwback converts the image, adds it to the first free slot, and writes the save back once you confirm. Your existing photos are kept. The new one shows up in the gallery and prints like any other.

Any PNG works. It's stretched to fit the camera's frame with no cropping, so crop it and adjust the brightness and contrast in an editor first for the best result. Dithering is on by default and looks best for photos. Pass `--no-dither` for logos or line art. `--slot` picks the slot and `--frame` sets the border.

`--from` and `-o` redirect either end:

```
throwback write-photo test.png --from camera.sav -o camera_new.sav   # file to file
throwback write-photo test.png -o camera_new.sav                     # read the cart, write a file
throwback write-photo test.png --from camera.sav                     # read a save file, write to the cart
```

A save file you build this way can be written to the cart later with `write-save`.

### write-rom

Write a ROM to a flash cart.

```
throwback write-rom homebrew.gbc
```

This erases the cart and writes the new ROM, so it only works on a flashable cart. Throwback checks the cart first and refuses to touch a retail game. Pass `--force` to write anyway. It asks you to confirm before erasing. Pass `-y` to skip. Hand it the file as-is.

### apply-patch

Apply an IPS, UPS, or BPS patch to a ROM, for homebrew updates or ROM hacks. The patch format is detected for you. Give it the patch file. The ROM comes from `--from` or the cart, and the result goes to `-o` or back onto the cart.

```
throwback apply-patch update.ips --from homebrew.gbc -o homebrew_patched.gbc   # file to file
throwback apply-patch hack.bps                                                 # patch the cart in place
throwback apply-patch update.ips -o patched.gbc                                # read the cart, patch to a file
```

Patching the cart in place flashes the result back, so it needs a flashable cart, the same as `write-rom`. `--force` overrides that check, and it asks you to confirm first.

UPS and BPS patches check that you fed in the right ROM and that the result came out correct. IPS patches are checked against the ROM's header instead. If a check fails, Throwback stops and writes nothing. Pass `--ignore-checksum` to apply anyway. A ROM with no header to check, like SNES, is written without that step.

### upgrade

Update a game to its latest version. Throwback checks an update service and applies the official update if there is one.

(Only ModRetro games are supported for now, through the ModRetro Cart Clinic service.)

With no arguments it reads the inserted cartridge, checks for an update, and flashes the new version back:

```
throwback upgrade
```

`--from` and `-o` redirect either end: read a ROM file instead of the cart, write the result to a file instead of flashing, or both.

```
throwback upgrade --from dragonyhm.gbc -o dragonyhm_latest.gbc   # file to file
throwback upgrade -o dragonyhm_latest.gbc                        # upgrade the cart's ROM to a file
throwback upgrade --check                                        # report only, change nothing
```

`--check` reports what's available and changes nothing. The upgraded ROM is checked before it's written. A service only updates games it recognizes. For anything else, Throwback says so and stops.

When the result goes to the cartridge, Throwback flashes it once you confirm. Pass `-y` to skip. If the update changes the save format, it says so and asks before flashing (or pass `--acknowledge-incompatible-save`). Back up the save first with `read-save` if you want to keep it. Flashing needs a flashable cart, the same as `write-rom`, and `--force` overrides the check.

## Worth knowing

- Clean the contacts. A dirty cartridge reads back corrupt data, and `info` can report an empty slot with a cart inserted.
- Writing a save or a ROM cannot be undone. Back up first.
- A few carts read back data that doesn't match the canonical database dumps. This is the hardware, not Throwback. The official software reads them the same way.
