# Throwback

A CLI for working with Epilogue's GB Operator and SN Operator.

It dumps ROMs, backs up and restores saves, reads and sets the real-time clock on Pokémon carts, pulls the photos off a Game Boy Camera, writes ROMs to flash carts, applies IPS, UPS, and BPS patches, and updates games to their latest version.

Throwback is an independent project. It talks to the Operator hardware over USB but is not affiliated with Epilogue.

## What it works with

- **GB Operator**: Game Boy, Game Boy Color, Game Boy Advance
- **SN Operator**: Super Nintendo / Super Famicom

Plug the Operator into USB and insert a cartridge. Throwback finds the device on its own; there's no port to pick or device to select.

## Install

macOS and Linux:

```
curl -fsSL https://raw.githubusercontent.com/nathankellenicki/throwback/main/scripts/install.sh | sh
```

Windows (PowerShell):

```
irm https://raw.githubusercontent.com/nathankellenicki/throwback/main/scripts/install.ps1 | iex
```

This drops Throwback in `~/.throwback/` and puts it on your PATH. Run `throwback` to start. Or grab a build from the [latest release](https://github.com/nathankellenicki/throwback/releases), unpack it, and run the binary directly.

## Building

Throwback is written in Rust. Rust 1.85 or newer required.

```
cargo build --release
```

The binary ends up at `target/release/throwback`. Copy it onto your PATH if you want to run it as `throwback`.

## Commands

Run `throwback` with no arguments to list the commands, or `throwback <command> --help` for the options on any one of them.

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

GBA carts are trimmed to their real size automatically. SNES carts whose size isn't a power of two (a 2.5 MB game that the hardware reads as 4 MB, for example) get trimmed back down as well.

### read-save and write-save

Copy the cartridge's save RAM to a file, and write it back later.

```
throwback read-save crystal.sav
throwback write-save crystal.sav
```

Saves are raw SRAM, the same format emulators use, so a backup moves between Throwback and an emulator without any conversion. `write-save` replaces the save on the cart, so keep the original somewhere safe.

### read-rtc and write-rtc

For Game Boy carts with a real-time clock (the MBC3 games, such as Pokémon Gold, Silver, and Crystal), read or set the clock.

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
throwback write-rtc --input crystal.rtc
```

If the cart's battery has died the clock reads back out-of-range values, and Throwback tells you so.

### read-camera

Pull the photos off a Game Boy Camera. Each photo is saved as a PNG in the directory you give it.

```
throwback read-camera photos/
```

The camera's images are tiny (128×112, or 160×144 framed), so each photo is scaled up 10x by default by duplicating pixels, with no smoothing. Pass `--scaling` to change the factor, like `--scaling 1x` for the native size or `--scaling 20x` for larger:

```
throwback read-camera photos/ --scaling 1x
```

Add `--framed` to render the full 160×144 screen with the border around each photo (the "Nintendo / Game Boy" frame, or the Hello Kitty borders if it's that cartridge). This reads the camera ROM too, since the frames live there.

```
throwback read-camera photos/ --framed
```

You can also decode a save file you already have instead of reading the cart. With `--framed` you then need to point it at a camera ROM as well:

```
throwback read-camera photos/ --from camera.sav
throwback read-camera photos/ --from camera.sav --framed --rom camera.gb
```

### write-rom

Write a ROM to a flash cart.

```
throwback write-rom homebrew.gbc
```

This erases the cart and writes the new ROM, so it only makes sense on a flashable cart. Throwback checks the cartridge first and refuses to touch a retail game. If you have a reason to write anyway, pass `--force`. The ROM is padded and aligned for you, so hand it the file as-is.

### apply-patch

Apply an IPS, UPS, or BPS patch to a ROM file, useful for homebrew updates or ROM hacks before writing them to a flash cart. The format is detected from the patch itself, not the file extension.

```
throwback apply-patch homebrew.gbc update.ips -o homebrew_patched.gbc
throwback apply-patch base.gba hack.bps -o hacked.gba
```

UPS and BPS patches carry checksums of the ROM they expect and the result they produce, so Throwback verifies you fed it the right base ROM before applying and that the output is correct. A mismatch is an error; pass `--ignore-checksum` to apply anyway.

IPS has no such checksums, so for IPS the patched ROM is instead checked against its Game Boy / Game Boy Color or Game Boy Advance header checksum. If that fails, the output is not written unless you pass `--ignore-checksum`. A ROM in another format (SNES, for example) has no header checksum Throwback can verify, so it's written without that check.

### upgrade

Update a game to its latest version. Throwback checks an update service for a newer version of your ROM and applies the official update if there is one.

(Currently only ModRetro games are supported, using the ModRetro Cart Clinic service.)

Point it at a ROM file:

```
throwback upgrade dragonyhm.gbc
throwback upgrade dragonyhm.gbc --check
```

`--check` reports what's available and changes nothing. Otherwise Throwback writes the upgraded ROM next to the input as `<title> <version>.gbc`, or wherever `-o` points, after verifying it. A service can only update games it recognizes; for anything else, Throwback tells you and stops.

Run it with no file to update the inserted cartridge:

```
throwback upgrade
```

This dumps the cart, checks for an update, and flashes the new version back once you confirm by typing `y`. Pass `--write` to skip that confirmation. If the update changes the save format, Throwback says so and asks you to confirm before flashing (or pass `--acknowledge-incompatible-save`); back up the save first with `read-save` if you want to keep it. Flashing needs a writeable flash cart, the same as `write-rom`; `--force` overrides the check.

## Worth knowing

- Clean the contacts. A dirty cartridge reads back corrupt data, and `info` can report an empty slot even with a cart inserted.
- Writing a save or a ROM cannot be undone. Back up first.
- A few carts read back data that doesn't match the canonical database dumps. This is the hardware's doing, not Throwback's; the official software reads them the same way.
