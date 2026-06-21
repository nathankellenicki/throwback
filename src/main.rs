use clap::{CommandFactory, Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use throwback::cartridge::{
    self, CartridgeInfo, CartridgeType, detect_eeprom_size, detect_gba_save, format_size,
    trim_gba_rom,
};
use throwback::device::{self, CartridgeDevice, ChipType};
use throwback::patch::{self, Patch};
use throwback::upgrade::{self, Identity, Upgrade};

const GBA_MAX_ROM: u32 = 32 * 1024 * 1024; // 32 MB

#[derive(Parser)]
#[command(
    name = "throwback",
    version,
    // We print `--version` ourselves (just the bare number); clap's built-in
    // flag would prefix the binary name.
    disable_version_flag = true,
    about = "CLI for the Epilogue GB/SN Operator",
    // Put the version on the second line, just under the intro.
    help_template = "{about-with-newline}Throwback {version}\n\n{usage-heading} {usage}\n\n{all-args}{after-help}"
)]
struct Cli {
    /// Print version
    #[arg(short = 'V', long)]
    version: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show cartridge info
    Info {
        /// Print raw signature bytes
        #[arg(long)]
        raw: bool,
    },
    /// Dump ROM to a file
    DumpRom {
        /// Output file path
        output: PathBuf,
    },
    /// Read save data to a file
    ReadSave {
        /// Output file path
        output: PathBuf,
        /// Write raw SRAM only, without appending the cartridge's RTC
        #[arg(long)]
        no_rtc: bool,
    },
    /// Write save data from a file
    WriteSave {
        /// Input file path
        input: PathBuf,
        /// Ignore any RTC bundled in the save file (write SRAM only)
        #[arg(long)]
        no_rtc: bool,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Write ROM to a flash cart
    WriteRom {
        /// Input file path
        input: PathBuf,
        /// Write even if the cartridge isn't detected as a writeable flashcart
        #[arg(long)]
        force: bool,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Apply an IPS, UPS, or BPS patch to a ROM (file or the inserted cart)
    ApplyPatch {
        /// IPS, UPS, or BPS patch file
        patch: PathBuf,
        /// Source ROM file; omit to read the inserted cartridge
        #[arg(long)]
        from: Option<PathBuf>,
        /// Output ROM path; omit to flash the result to the inserted cartridge
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Write/flash even if the patched ROM fails checksum validation
        #[arg(long)]
        ignore_checksum: bool,
        /// Cart mode: flash even if the cart isn't a detected writeable flashcart
        #[arg(long)]
        force: bool,
        /// Cart mode: skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Check update services for a newer version of a ROM (or the inserted cart) and apply it
    Upgrade {
        /// ROM file to upgrade; omit to read the inserted cartridge
        #[arg(long)]
        from: Option<PathBuf>,
        /// Output ROM path; omit to flash the upgrade to the inserted cartridge
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Report available updates without applying or flashing anything
        #[arg(long)]
        check: bool,
        /// Proceed past a save-incompatible update without the interactive prompt
        #[arg(long)]
        acknowledge_incompatible_save: bool,
        /// Skip verification of the upgraded ROM
        #[arg(long)]
        ignore_checksum: bool,
        /// Cart mode: flash even if the cart isn't a detected writeable flashcart
        #[arg(long)]
        force: bool,
        /// Cart mode: skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Read the photos off a Game Boy Camera (cartridge or save file)
    ReadPhotos {
        /// Output directory for the decoded PNGs
        output: PathBuf,
        /// Decode an existing camera save file instead of reading the cartridge
        #[arg(long)]
        from: Option<PathBuf>,
        /// Render the on-device 160x144 view with each photo's decorative frame
        #[arg(long)]
        framed: bool,
        /// Camera ROM file for --framed (with --from); otherwise read from the cart
        #[arg(long)]
        rom: Option<PathBuf>,
        /// Scale each exported image by an integer factor via pixel duplication, e.g. 2x or 10x
        #[arg(long, value_parser = parse_scaling, default_value = "10x")]
        scaling: u32,
    },
    /// Add a photo to a Game Boy Camera (cartridge or save file)
    WritePhoto {
        /// PNG to add (any size/color; converted to the camera's 128x112 4-shade format)
        image: PathBuf,
        /// Camera save file to inject into; omit to read the inserted cartridge
        #[arg(long)]
        from: Option<PathBuf>,
        /// Output save path; omit to write the result back to the inserted cartridge
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Target photo slot 0..=29 (default: the first free slot)
        #[arg(long)]
        slot: Option<usize>,
        /// Border/frame index stored with the photo
        #[arg(long, default_value_t = 0)]
        frame: u8,
        /// Map each pixel to the nearest shade instead of dithering (better for logos/line art)
        #[arg(long)]
        no_dither: bool,
        /// Cart mode: skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Read the cartridge's real-time clock (MBC3 carts)
    ReadRtc {
        /// Optional file to save the raw 40-byte RTC payload as a backup
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Write the cartridge's real-time clock (MBC3 carts)
    WriteRtc {
        /// Restore from a raw 40-byte .rtc backup (takes precedence over the flags)
        #[arg(long)]
        from: Option<PathBuf>,
        /// Day counter to set
        #[arg(long, default_value_t = 0)]
        days: u16,
        /// Hours to set (0-23)
        #[arg(long, default_value_t = 0)]
        hours: u8,
        /// Minutes to set (0-59)
        #[arg(long, default_value_t = 0)]
        minutes: u8,
        /// Seconds to set (0-59)
        #[arg(long, default_value_t = 0)]
        seconds: u8,
        /// Skip the confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
}

fn open_device() -> Box<dyn CartridgeDevice> {
    match device::open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: {e}");
            process::exit(1);
        }
    }
}

fn read_cart_info(device: &mut dyn CartridgeDevice) -> CartridgeInfo {
    match device.read_cartridge_info() {
        Ok(data) => {
            let info = CartridgeInfo::from_bytes(&data);
            if !info.present {
                eprintln!("No cartridge inserted.");
                process::exit(1);
            }
            info
        }
        Err(e) => {
            eprintln!("Error reading cartridge: {e}");
            process::exit(1);
        }
    }
}

fn print_progress(label: &str, current: u32, total: u32) {
    let pct = (current as f64 / total as f64 * 100.0) as u32;
    eprint!(
        "\r{label}: {}/{} ({pct}%)    ",
        format_size(current),
        format_size(total)
    );
    if current >= total {
        eprintln!();
    }
}

/// Device + cart info, opened when the cartridge is the source or destination.
struct CartHandle {
    device: Box<dyn CartridgeDevice>,
    info: CartridgeInfo,
}

/// Open the device when the cartridge is needed on either end (source or destination).
/// When both `from` and `output` are files the operation is entirely offline, so this
/// returns `None` and never touches the hardware.
fn open_if_cart(from: Option<&Path>, output: Option<&Path>) -> Option<CartHandle> {
    if from.is_some() && output.is_some() {
        return None;
    }
    let mut device = open_device();
    let info = read_cart_info(device.as_mut());
    Some(CartHandle { device, info })
}

/// Read the source ROM from a file, or dump it from the inserted cart.
fn read_rom_source(from: Option<&Path>, handle: Option<&mut CartHandle>) -> Vec<u8> {
    match from {
        Some(p) => fs::read(p).unwrap_or_else(|e| {
            eprintln!("Error reading ROM: {e}");
            process::exit(1);
        }),
        None => {
            let h = handle.expect("cart handle is opened when the source is the cartridge");
            dump_cart_rom(h.device.as_mut(), &h.info)
        }
    }
}

/// Flashcart-writeable guard, pad to a 64-byte boundary, then flash. The caller is
/// responsible for any confirmation prompt before calling this.
fn flash_rom(handle: &mut CartHandle, rom: Vec<u8>, force: bool) {
    if !force {
        match handle.device.detect_flashcart() {
            Ok(d) if !cartridge::flashcart_writeable(&d) => {
                eprintln!(
                    "Refusing to write: this cartridge is not a writeable flashcart (retail / mask ROM)."
                );
                eprintln!("Pass --force to override.");
                process::exit(1);
            }
            _ => {}
        }
    }
    let mut padded = rom;
    if !padded.len().is_multiple_of(64) {
        padded.resize(padded.len() + (64 - padded.len() % 64), 0xFF);
    }
    let ram_size = handle.info.ram_size;
    eprintln!("Writing {} to the cartridge...", format_size(padded.len() as u32));
    if let Err(e) = handle.device.write_rom(
        &padded,
        ram_size,
        &|cur| print_progress("Writing", cur, padded.len() as u32),
        &|msg| eprintln!("\r{msg}    "),
    ) {
        eprintln!("\nError: {e}");
        process::exit(1);
    }
}

/// Load + apply + verify a patch, returning the patched ROM. Verification messages are
/// printed here; the caller decides where the result goes (file or cart).
fn apply_patch_bytes(rom: Vec<u8>, patch_path: &Path, ignore_checksum: bool) -> Vec<u8> {
    let patch_data = fs::read(patch_path).unwrap_or_else(|e| {
        eprintln!("Error reading patch: {e}");
        process::exit(1);
    });

    let patch = Patch::load(&patch_data).unwrap_or_else(|e| {
        eprintln!("Error parsing patch: {e}");
        process::exit(1);
    });

    let format = patch.format_name();
    // UPS/BPS verify the source ROM's CRC32 before applying (a mismatch is a hard
    // error unless --ignore-checksum). IPS has no such check and applies in place.
    let patched = patch.apply_into(rom, !ignore_checksum).unwrap_or_else(|e| {
        eprintln!("Error applying {format} patch: {e}");
        if !ignore_checksum {
            eprintln!("Pass --ignore-checksum to bypass checksum verification.");
        }
        process::exit(1);
    });

    // An empty result (e.g. a truncate-to-0 patch) is never a usable ROM, so
    // refuse it regardless of --ignore-checksum.
    if patched.is_empty() {
        eprintln!("Patch produced an empty ROM; refusing to write.");
        process::exit(1);
    }

    if ignore_checksum {
        // verification skipped entirely
    } else if patch.has_checksums() {
        // UPS/BPS already verified source and target CRC32 inside apply.
        eprintln!("{format} source and target checksums OK.");
    } else {
        // IPS: fall back to the header-checksum heuristic.
        match patch::validate_patched_rom(&patched) {
            patch::Validation::Ok => eprintln!("Header checksum OK."),
            patch::Validation::Skipped(reason) => {
                eprintln!("Skipping header-checksum validation: {reason}.");
            }
            patch::Validation::Mismatch(e) => {
                eprintln!("Validation failed: {e}");
                eprintln!("The patch may be for a different ROM or the result may be corrupt.");
                eprintln!("Pass --ignore-checksum to write it anyway.");
                process::exit(1);
            }
        }
    }

    eprintln!("Applied {} patch.", format);
    patched
}

/// Run the service sweep over `rom` and report. Returns `Some((id, upgrade))` when a
/// newer version is available; otherwise prints the outcome and either returns `None`
/// (already-latest / unknown game) or exits non-zero (service or fetch failure).
fn resolve_update(rom: &[u8]) -> Option<(Identity, Upgrade)> {
    eprintln!("Checking update services...");
    let services = upgrade::services();
    let resolution = upgrade::resolve(rom, &services, |name| eprintln!("Checking {name}..."));
    match resolution {
        upgrade::Resolution::AlreadyLatest(id) => {
            println!("{} is already the latest version ({}).", id.title, id.current_version);
            None
        }
        upgrade::Resolution::Update(id, up) => Some((id, up)),
        upgrade::Resolution::Failed(id, e) => {
            eprintln!("Update check failed for {}: {e}", id.title);
            process::exit(1);
        }
        upgrade::Resolution::Unrecognized(errors) => {
            if errors.is_empty() {
                println!("No update service recognized this ROM.");
            } else {
                eprintln!("No update service recognized this ROM. Service errors:");
                for (name, e) in errors {
                    eprintln!("  {name}: {e}");
                }
                process::exit(1);
            }
            None
        }
    }
}

fn print_update_summary(id: &Identity, up: &Upgrade) {
    println!();
    println!(
        "{}: {} -> {} available (via {})",
        id.title, up.from_version, up.to_version, id.service
    );
    if up.save_compatible == Some(false) {
        println!("Saves are NOT compatible across this update.");
    }
    if let Some(changelog) = &up.changelog {
        println!();
        for line in changelog.lines() {
            println!("{line}");
        }
    }
}

/// Dump the inserted cart's ROM into memory (GBA is auto-trimmed), for the sweep.
fn dump_cart_rom(device: &mut dyn CartridgeDevice, info: &CartridgeInfo) -> Vec<u8> {
    let mut read = |chip, size: u32, save| {
        device.read_rom(chip, size, save, &|cur| print_progress("Reading", cur, size))
            .unwrap_or_else(|e| {
                eprintln!("\nError: {e}");
                process::exit(1);
            })
    };
    match info.cart_type {
        CartridgeType::GB => {
            eprintln!("Reading cartridge ROM ({})...", format_size(info.rom_size));
            read(ChipType::Unknown, info.rom_size, info.ram_size)
        }
        CartridgeType::GBA => {
            eprintln!("Reading GBA ROM (up to {}, will auto-trim)...", format_size(GBA_MAX_ROM));
            let rom = read(ChipType::Unknown, GBA_MAX_ROM, 0);
            let n = trim_gba_rom(&rom);
            rom[..n].to_vec()
        }
        CartridgeType::SNES => {
            eprintln!("Reading SNES ROM ({})...", format_size(info.rom_size));
            read(ChipType::Unknown, info.rom_size, 0)
        }
    }
}

/// Prompt for a typed `y` on the terminal. Returns false (without blocking) when
/// stdin isn't a TTY — automation should pass the relevant bypass flag instead.
fn confirm(prompt: &str) -> bool {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        eprintln!("{prompt} [y/N] — no terminal; pass the bypass flag for automation.");
        return false;
    }
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes" | "Yes")
}

/// `upgrade`: source ROM from a file (`--from`) or the cart; result to a file (`-o`) or
/// flashed back to the cart. With neither flag it reads and re-flashes the inserted cart.
#[allow(clippy::fn_params_excessive_bools)]
fn do_upgrade(
    from: Option<&Path>,
    output: Option<&Path>,
    check: bool,
    yes: bool,
    acknowledge_incompatible_save: bool,
    ignore_checksum: bool,
    force: bool,
) {
    if let (Some(o), Some(f)) = (output, from)
        && o == f
    {
        eprintln!("Output is the same as the input; pass a different -o <path>.");
        process::exit(1);
    }

    let mut handle = open_if_cart(from, output);
    let rom = read_rom_source(from, handle.as_mut());

    let Some((id, up)) = resolve_update(&rom) else {
        return;
    };
    print_update_summary(&id, &up);
    if check {
        return;
    }
    println!();

    let upgraded = upgrade::produce_upgraded_rom(&up, rom, !ignore_checksum).unwrap_or_else(|e| {
        eprintln!("Error applying update: {e}");
        if !ignore_checksum {
            eprintln!("Pass --ignore-checksum to bypass verification.");
        }
        process::exit(1);
    });

    match output {
        // File destination: write and stop, no hardware involved.
        Some(out) => {
            fs::write(out, &upgraded).unwrap_or_else(|e| {
                eprintln!("Error writing output: {e}");
                process::exit(1);
            });
            eprintln!(
                "Upgraded {} {} -> {}; wrote {}.",
                id.title,
                up.from_version,
                up.to_version,
                out.display()
            );
        }
        // Cart destination: acknowledge save incompatibility, confirm, then flash.
        None => {
            let h = handle.as_mut().expect("cart handle is opened when flashing");

            // Save-incompatibility acknowledgement (only matters if the cart holds a save).
            if up.save_compatible == Some(false)
                && h.info.ram_size > 0
                && !acknowledge_incompatible_save
            {
                eprintln!(
                    "This update is NOT save-compatible; the save on your cartridge may not carry over."
                );
                eprintln!("Back up the save first with `throwback read-save` if you want to keep it.");
                if !confirm("Continue and flash anyway?") {
                    eprintln!("Aborted.");
                    process::exit(1);
                }
            }

            if !yes
                && !confirm(&format!(
                    "Flash {} {} to the cartridge? This erases it.",
                    id.title, up.to_version
                ))
            {
                eprintln!("Aborted.");
                process::exit(1);
            }

            flash_rom(h, upgraded, force);
            eprintln!("Upgraded {} to {} on the cartridge.", id.title, up.to_version);
        }
    }

    if ignore_checksum {
        eprintln!("Warning: skipped verification.");
    }
}

/// Parse a `--scaling` value like "2x", "10x", or a bare "3" into an integer factor.
fn parse_scaling(s: &str) -> Result<u32, String> {
    let digits = s.strip_suffix(['x', 'X']).unwrap_or(s);
    let factor: u32 = digits
        .parse()
        .map_err(|_| format!("invalid scaling '{s}'; use a factor like 2x or 10"))?;
    if factor < 1 {
        return Err("scaling must be at least 1x".to_string());
    }
    if factor > 1000 {
        return Err("scaling must be at most 1000x".to_string());
    }
    Ok(factor)
}

/// Load a PNG and convert it to a 128×112 image for the Game Boy Camera: grayscale,
/// box-averaged down to camera resolution, then reduced to the four camera shades —
/// Floyd–Steinberg dithered when `dither` is set (good for photos), otherwise mapped
/// straight to the nearest shade (good for logos/line art).
fn load_camera_image(path: &Path, dither: bool) -> Result<Vec<u8>, String> {
    let file = fs::File::open(path).map_err(|e| format!("open image: {e}"))?;
    let mut decoder = png::Decoder::new(file);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| format!("read PNG: {e}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("decode PNG: {e}"))?;
    let (sw, sh) = (info.width as usize, info.height as usize);
    if sw == 0 || sh == 0 {
        return Err("image has zero dimensions".to_string());
    }
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err("indexed PNGs aren't supported".to_string()),
    };
    let data = &buf[..info.buffer_size()];
    let luma = |x: usize, y: usize| -> u32 {
        let i = (y * sw + x) * channels;
        match channels {
            1 | 2 => data[i] as u32,
            _ => (data[i] as u32 * 299 + data[i + 1] as u32 * 587 + data[i + 2] as u32 * 114) / 1000,
        }
    };

    let (tw, th) = (cartridge::CAMERA_PHOTO_WIDTH, cartridge::CAMERA_PHOTO_HEIGHT);
    // Box-average downscale to camera resolution.
    let mut gray = vec![0i32; tw * th];
    for ty in 0..th {
        let (y0, y1) = (ty * sh / th, (((ty + 1) * sh / th).max(ty * sh / th + 1)).min(sh));
        for tx in 0..tw {
            let (x0, x1) = (tx * sw / tw, (((tx + 1) * sw / tw).max(tx * sw / tw + 1)).min(sw));
            let (mut sum, mut n) = (0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += luma(x, y);
                    n += 1;
                }
            }
            gray[ty * tw + tx] = (sum / n.max(1)) as i32;
        }
    }

    // Reduce to the four GB Camera shades.
    let shades = [0i32, 0x54, 0xA8, 0xFF];
    let nearest = |v: i32| *shades.iter().min_by_key(|&&s| (v - s).abs()).unwrap();
    let mut out = vec![0u8; tw * th];
    if dither {
        // Floyd–Steinberg: snap to nearest shade, diffuse the error to neighbours.
        for y in 0..th {
            for x in 0..tw {
                let old = gray[y * tw + x].clamp(0, 255);
                let newv = nearest(old);
                out[y * tw + x] = newv as u8;
                let err = old - newv;
                let mut spread = |xx: usize, yy: usize, f: i32| gray[yy * tw + xx] += err * f / 16;
                if x + 1 < tw {
                    spread(x + 1, y, 7);
                }
                if y + 1 < th {
                    if x > 0 {
                        spread(x - 1, y + 1, 3);
                    }
                    spread(x, y + 1, 5);
                    if x + 1 < tw {
                        spread(x + 1, y + 1, 1);
                    }
                }
            }
        }
    } else {
        // Plain thresholding: each pixel to its nearest shade.
        for (o, &g) in out.iter_mut().zip(gray.iter()) {
            *o = nearest(g.clamp(0, 255)) as u8;
        }
    }
    Ok(out)
}

/// Pick the camera slot to write: the requested one, or the first free slot.
fn pick_camera_slot(save: &[u8], requested: Option<usize>) -> usize {
    if let Some(s) = requested {
        return s;
    }
    (0..30)
        .find(|&s| save.get(0x11B2 + s) == Some(&0xFF))
        .unwrap_or_else(|| {
            eprintln!("The camera is full (30 photos). Pass --slot to overwrite one.");
            process::exit(1);
        })
}

/// Write an 8-bit grayscale buffer as a PNG.
fn write_gray_png(
    path: &std::path::Path,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let file = fs::File::create(path)?;
    let buf = std::io::BufWriter::new(file);
    let mut enc = png::Encoder::new(buf, width, height);
    enc.set_color(png::ColorType::Grayscale);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(pixels)?;
    Ok(())
}

fn dump_rom_gb(device: &mut dyn CartridgeDevice, info: &CartridgeInfo, output: &PathBuf) {
    eprintln!("Dumping GB ROM ({})...", format_size(info.rom_size));

    match device.read_rom(ChipType::Unknown, info.rom_size, info.ram_size, &|cur| {
        print_progress("Reading", cur, info.rom_size);
    }) {
        Ok(rom) => {
            fs::write(output, &rom).unwrap_or_else(|e| {
                eprintln!("Error writing file: {e}");
                process::exit(1);
            });
            eprintln!("Wrote {}.", output.display());
        }
        Err(e) => {
            eprintln!("\nError: {e}");
            process::exit(1);
        }
    }
}

fn dump_rom_gba(device: &mut dyn CartridgeDevice, _info: &CartridgeInfo, output: &PathBuf) {
    eprintln!(
        "Dumping GBA ROM (reading {} max, will auto-trim)...",
        format_size(GBA_MAX_ROM)
    );

    match device.read_rom(ChipType::Unknown, GBA_MAX_ROM, 0, &|cur| {
        print_progress("Reading", cur, GBA_MAX_ROM);
    }) {
        Ok(rom) => {
            let trimmed_size = trim_gba_rom(&rom);
            let trimmed = &rom[..trimmed_size];

            eprintln!(
                "Trimmed to {} (actual ROM data)",
                format_size(trimmed_size as u32)
            );

            fs::write(output, trimmed).unwrap_or_else(|e| {
                eprintln!("Error writing file: {e}");
                process::exit(1);
            });
            eprintln!("Wrote {}.", output.display());
        }
        Err(e) => {
            eprintln!("\nError: {e}");
            process::exit(1);
        }
    }
}

fn dump_rom_snes(device: &mut dyn CartridgeDevice, info: &CartridgeInfo, output: &PathBuf) {
    if info.rom_size == 0 {
        eprintln!("Could not determine SNES ROM size from the cartridge signature.");
        process::exit(1);
    }

    eprintln!("Dumping SNES ROM ({})...", format_size(info.rom_size));

    let rom = match device.read_rom(ChipType::Unknown, info.rom_size, 0, &|cur| {
        print_progress("Reading", cur, info.rom_size);
    }) {
        Ok(rom) => rom,
        Err(e) => {
            eprintln!("\nError: {e}");
            process::exit(1);
        }
    };

    // Parse the dumped header to confirm the dump and report details. This both
    // validates the size the device reported and surfaces title/mapper/save info.
    match cartridge::parse_snes_header(&rom) {
        Some(header) => eprintln!("{header}"),
        None => eprintln!(
            "Warning: no valid SNES header found in the dump — the ROM may be \
             incomplete or use an unrecognized mapper."
        ),
    }

    // The signature rounds non-power-of-2 carts up to the next power of two, so the
    // read is padded with an open-bus/mirror tail. Trim it back to the real size.
    let trimmed = cartridge::trim_snes_rom(&rom);
    let out = if trimmed < rom.len() {
        eprintln!(
            "Trimmed non-power-of-2 ROM: {} -> {} (dropped over-read tail)",
            format_size(rom.len() as u32),
            format_size(trimmed as u32)
        );
        &rom[..trimmed]
    } else {
        &rom[..]
    };

    fs::write(output, out).unwrap_or_else(|e| {
        eprintln!("Error writing file: {e}");
        process::exit(1);
    });
    eprintln!("Wrote {}.", output.display());
}

fn main() {
    let cli = Cli::parse();

    if cli.version {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    }

    // No subcommand: show the help (with the version line) and exit, matching
    // clap's behaviour for a missing required subcommand.
    let Some(command) = cli.command else {
        let _ = Cli::command().print_help();
        process::exit(2);
    };

    match command {
        Commands::Info { raw } => {
            let mut device = open_device();
            let data = match device.read_cartridge_info() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("Error reading cartridge: {e}");
                    process::exit(1);
                }
            };

            if raw {
                for (i, chunk) in data.chunks(16).enumerate() {
                    eprint!("{:04X}  ", i * 16);
                    for b in chunk {
                        eprint!("{:02X} ", b);
                    }
                    eprint!(" ");
                    for b in chunk {
                        let c = if b.is_ascii_graphic() || *b == b' ' {
                            *b as char
                        } else {
                            '.'
                        };
                        eprint!("{c}");
                    }
                    eprintln!();
                }
            }

            let mut info = CartridgeInfo::from_bytes(&data);
            if !info.present {
                eprintln!("No cartridge inserted.");
                process::exit(1);
            }

            // Read just the cartridge header (not the whole ROM) to surface the full
            // title. For SNES this also yields mapper + save size, so print the richer
            // SnesHeader when it parses; otherwise fall back to the signature view.
            match info.cart_type {
                CartridgeType::SNES => {
                    match device
                        .read_header()
                        .ok()
                        .and_then(|buf| cartridge::parse_snes_header(&buf))
                    {
                        Some(header) => println!("{header}"),
                        None => println!("{info}"),
                    }
                }
                CartridgeType::GB => {
                    if let Ok(buf) = device.read_header() {
                        info.title = cartridge::parse_gb_title(&buf);
                        info.type_label = Some(cartridge::parse_cgb_flag(&buf).to_string());
                        info.region_label = cartridge::parse_gb_region(&buf).map(String::from);
                        if buf.len() > 0x14D {
                            info.header_checksum = buf[0x14D];
                            info.checksum_valid =
                                cartridge::gb_header_checksum(&buf).map(|c| c == buf[0x14D]);
                            info.version = Some(buf[0x14C]);
                        }
                    }
                    println!("{info}");
                }
                CartridgeType::GBA => {
                    if let Ok(buf) = device.read_header() {
                        info.title = cartridge::parse_gba_title(&buf);
                        info.region_label = cartridge::parse_gba_region(&buf);
                        if buf.len() > 0xBD {
                            info.header_checksum = buf[0xBD];
                            info.checksum_valid =
                                cartridge::gba_header_checksum(&buf).map(|c| c == buf[0xBD]);
                            info.version = Some(buf[0xBC]);
                        }
                    }
                    println!("{info}");
                }
            }

            // Read-only flashcart probe: is this a writeable flashcart or a retail cart?
            if let Ok(detect) = device.detect_flashcart() {
                if cartridge::flashcart_writeable(&detect) {
                    println!("Writeable:       Yes (flashcart)");
                } else {
                    println!("Writeable:       No (retail/mask ROM)");
                }
            }
        }

        Commands::DumpRom { output } => {
            let mut device = open_device();
            let info = read_cart_info(device.as_mut());

            match info.cart_type {
                CartridgeType::GB => dump_rom_gb(device.as_mut(), &info, &output),
                CartridgeType::GBA => dump_rom_gba(device.as_mut(), &info, &output),
                CartridgeType::SNES => dump_rom_snes(device.as_mut(), &info, &output),
            }
        }

        Commands::ReadSave { output, no_rtc } => {
            let mut device = open_device();
            let info = read_cart_info(device.as_mut());

            match info.cart_type {
                CartridgeType::GB => {
                    if info.ram_size == 0 {
                        eprintln!("This cartridge has no save RAM.");
                        process::exit(1);
                    }

                    eprintln!("Reading save ({})...", format_size(info.ram_size));

                    let mut save = match device.read_save(
                        ChipType::Unknown,
                        info.rom_size,
                        info.ram_size,
                        &|cur| print_progress("Reading", cur, info.ram_size),
                    ) {
                        Ok(save) => save,
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        }
                    };

                    // Bundle the clock into the .sav so it's a complete, emulator-
                    // compatible backup (unless the cart has no RTC or --no-rtc).
                    if info.has_rtc() && !no_rtc {
                        match device.read_rtc(info.rom_size, info.ram_size) {
                            Ok(payload) if payload.len() >= RTC_REGS_LEN => {
                                append_rtc_trailer(&mut save, &payload);
                                eprintln!("Appended RTC (48 bytes).");
                            }
                            Ok(_) => eprintln!(
                                "Warning: RTC read returned too few bytes; wrote SRAM only."
                            ),
                            Err(e) => {
                                eprintln!("Warning: couldn't read the RTC ({e}); wrote SRAM only.");
                            }
                        }
                    }

                    fs::write(&output, &save).unwrap_or_else(|e| {
                        eprintln!("Error writing file: {e}");
                        process::exit(1);
                    });
                    eprintln!("Wrote {}.", output.display());
                }
                CartridgeType::GBA => {
                    eprintln!("Dumping ROM to detect save type...");

                    let rom = match device.read_rom(ChipType::Unknown, GBA_MAX_ROM, 0, &|cur| {
                        print_progress("Reading ROM", cur, GBA_MAX_ROM);
                    }) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        }
                    };

                    let (chip, save_size) = detect_gba_save(&rom);

                    if chip == ChipType::Unknown || save_size == 0 {
                        eprintln!("Could not detect save type for this GBA cartridge.");
                        process::exit(1);
                    }

                    let rom_size = trim_gba_rom(&rom) as u32;
                    eprintln!("Detected save: {:?}, {}", chip, format_size(save_size));
                    eprintln!("Reading save...");

                    match device.read_save(chip, rom_size, save_size, &|cur| {
                        print_progress("Reading", cur, save_size);
                    }) {
                        Ok(save) => {
                            // For EEPROM: detect actual size by checking for mirroring
                            let save = if chip == ChipType::Eeprom {
                                let trimmed = detect_eeprom_size(&save);
                                if trimmed.len() < save.len() {
                                    eprintln!(
                                        "EEPROM mirror detected: actual size is {}",
                                        format_size(trimmed.len() as u32)
                                    );
                                }
                                trimmed
                            } else {
                                save
                            };

                            fs::write(&output, &save).unwrap_or_else(|e| {
                                eprintln!("Error writing file: {e}");
                                process::exit(1);
                            });
                            eprintln!("Wrote {}.", output.display());
                        }
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        }
                    }
                }
                CartridgeType::SNES => {
                    // The signature reports ROM size but not SRAM size, so read the
                    // header and let parse_snes_header give us the save size + chip.
                    let header = match device
                        .read_header()
                        .ok()
                        .and_then(|buf| cartridge::parse_snes_header(&buf))
                    {
                        Some(h) => h,
                        None => {
                            eprintln!("Could not read the SNES cartridge header.");
                            process::exit(1);
                        }
                    };

                    if header.ram_size == 0 {
                        eprintln!("This cartridge has no save RAM.");
                        process::exit(1);
                    }

                    eprintln!("Reading save ({})...", format_size(header.ram_size));

                    match device.read_save(
                        header.save_chip,
                        header.rom_size,
                        header.ram_size,
                        &|cur| print_progress("Reading", cur, header.ram_size),
                    ) {
                        Ok(save) => {
                            fs::write(&output, &save).unwrap_or_else(|e| {
                                eprintln!("Error writing file: {e}");
                                process::exit(1);
                            });
                            eprintln!("Wrote {}.", output.display());
                        }
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::WriteSave { input, no_rtc, yes } => {
            let mut device = open_device();
            let info = read_cart_info(device.as_mut());

            let data = fs::read(&input).unwrap_or_else(|e| {
                eprintln!("Error reading file: {e}");
                process::exit(1);
            });

            if !yes && !confirm("Overwrite the save on the cartridge? This cannot be undone.") {
                eprintln!("Aborted.");
                process::exit(1);
            }

            match info.cart_type {
                CartridgeType::GB => {
                    if info.ram_size == 0 {
                        eprintln!("This cartridge has no save RAM.");
                        process::exit(1);
                    }

                    // Split off any RTC bundled into the .sav (VBA-M/mGBA append it).
                    let ram = info.ram_size as usize;
                    let (sram, trailer): (&[u8], Option<&[u8]>) = match data.len().checked_sub(ram) {
                        Some(0) => (&data, None),
                        Some(n) if RTC_TRAILER_SIZES.contains(&n) => {
                            (&data[..ram], Some(&data[ram..]))
                        }
                        _ => {
                            eprintln!(
                                "Warning: file size ({}) doesn't match cartridge RAM size ({}).",
                                format_size(data.len() as u32),
                                format_size(info.ram_size)
                            );
                            (&data, None)
                        }
                    };

                    eprintln!("Writing save ({})...", format_size(sram.len() as u32));

                    if let Err(e) = device.write_save(ChipType::Unknown, info.rom_size, sram, &|cur| {
                        print_progress("Writing", cur, sram.len() as u32);
                    }) {
                        eprintln!("\nError: {e}");
                        process::exit(1);
                    }
                    eprintln!("Save written.");

                    // Restore a bundled clock if present and the cart can hold one.
                    if let Some(trailer) = trailer
                        && !no_rtc
                    {
                        if !info.has_rtc() {
                            eprintln!(
                                "Note: this save includes a real-time clock, but the cartridge \
                                 has no RTC; skipped the clock."
                            );
                        } else if let Some(rtc) = cartridge::RtcData::parse(trailer) {
                            eprintln!("Writing bundled RTC...");
                            match device.write_rtc(info.rom_size, info.ram_size, &rtc.to_payload()) {
                                Ok(()) => eprintln!("RTC written ({rtc})."),
                                Err(e) => eprintln!("Warning: couldn't write the RTC ({e})."),
                            }
                        } else {
                            eprintln!("Warning: couldn't parse the bundled RTC; skipped the clock.");
                        }
                    }
                }
                CartridgeType::GBA => {
                    eprintln!("Dumping ROM to detect save type...");

                    let rom = match device.read_rom(ChipType::Unknown, GBA_MAX_ROM, 0, &|cur| {
                        print_progress("Reading ROM", cur, GBA_MAX_ROM);
                    }) {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        }
                    };

                    let (chip, save_size) = detect_gba_save(&rom);

                    if chip == ChipType::Unknown || save_size == 0 {
                        eprintln!("Could not detect save type for this GBA cartridge.");
                        process::exit(1);
                    }

                    let rom_size = trim_gba_rom(&rom) as u32;

                    if data.len() != save_size as usize {
                        eprintln!(
                            "Warning: file size ({}) doesn't match detected save size ({}).",
                            format_size(data.len() as u32),
                            format_size(save_size)
                        );
                    }

                    eprintln!("Detected save: {:?}, {}", chip, format_size(save_size));
                    eprintln!("Writing save ({})...", format_size(data.len() as u32));

                    match device.write_save(chip, rom_size, &data, &|cur| {
                        print_progress("Writing", cur, data.len() as u32);
                    }) {
                        Ok(()) => eprintln!("Save written."),
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        }
                    }
                }
                CartridgeType::SNES => {
                    // SRAM size comes from the header, not the signature.
                    let header = match device
                        .read_header()
                        .ok()
                        .and_then(|buf| cartridge::parse_snes_header(&buf))
                    {
                        Some(h) => h,
                        None => {
                            eprintln!("Could not read the SNES cartridge header.");
                            process::exit(1);
                        }
                    };

                    if header.ram_size == 0 {
                        eprintln!("This cartridge has no save RAM.");
                        process::exit(1);
                    }

                    if data.len() != header.ram_size as usize {
                        eprintln!(
                            "Warning: file size ({}) doesn't match cartridge SRAM size ({}).",
                            format_size(data.len() as u32),
                            format_size(header.ram_size)
                        );
                    }

                    eprintln!("Writing save ({})...", format_size(data.len() as u32));

                    match device.write_save(header.save_chip, header.rom_size, &data, &|cur| {
                        print_progress("Writing", cur, data.len() as u32);
                    }) {
                        Ok(()) => eprintln!("Save written."),
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::WriteRom { input, force, yes } => {
            let mut device = open_device();
            // The save size is part of the WriteGame command (Playback sends it), so
            // read the cartridge signature first to learn it.
            let info = read_cart_info(device.as_mut());

            // Safety guard: WriteGame erases the cart. A retail mask-ROM cart can't be
            // flashed, so refuse unless --force. (DetectFlashcart is read-only.)
            if !force {
                match device.detect_flashcart() {
                    Ok(d) if !cartridge::flashcart_writeable(&d) => {
                        eprintln!(
                            "Refusing to write: this cartridge is not a writeable flashcart \
                             (retail / mask ROM)."
                        );
                        eprintln!("Flashing it would fail and could corrupt the cart. Pass --force to override.");
                        process::exit(1);
                    }
                    _ => {} // writeable flashcart, or detection unsupported (e.g. SN Operator)
                }
            }

            let data = fs::read(&input).unwrap_or_else(|e| {
                eprintln!("Error reading file: {e}");
                process::exit(1);
            });

            if !yes && !confirm("Write this ROM to the cartridge? This erases it.") {
                eprintln!("Aborted.");
                process::exit(1);
            }

            // Pad to 64-byte boundary
            let mut padded = data.clone();
            if padded.len() % 64 != 0 {
                padded.resize(padded.len() + (64 - padded.len() % 64), 0xFF);
            }

            eprintln!("Writing {} to flash cart...", format_size(padded.len() as u32));

            match device.write_rom(&padded, info.ram_size, &|cur| {
                print_progress("Writing", cur, padded.len() as u32);
            }, &|msg| {
                eprintln!("\r{}    ", msg);
            }) {
                Ok(()) => {
                    eprintln!("ROM written.");
                }
                Err(e) => {
                    eprintln!("\nError: {e}");
                    process::exit(1);
                }
            }
        }

        Commands::ApplyPatch { patch, from, output, ignore_checksum, force, yes } => {
            if let (Some(o), Some(f)) = (output.as_deref(), from.as_deref())
                && o == f
            {
                eprintln!("Output is the same as the input; pass a different -o <path>.");
                process::exit(1);
            }

            let mut handle = open_if_cart(from.as_deref(), output.as_deref());
            let rom = read_rom_source(from.as_deref(), handle.as_mut());
            let patched = apply_patch_bytes(rom, &patch, ignore_checksum);

            match output.as_deref() {
                // File destination.
                Some(out) => {
                    fs::write(out, &patched).unwrap_or_else(|e| {
                        eprintln!("Error writing output: {e}");
                        process::exit(1);
                    });
                    eprintln!("Wrote {} ({}).", out.display(), format_size(patched.len() as u32));
                }
                // Cart destination: confirm, then flash.
                None => {
                    let h = handle.as_mut().expect("cart handle is opened when flashing");
                    if !yes && !confirm("Flash the patched ROM to the cartridge? This erases it.") {
                        eprintln!("Aborted.");
                        process::exit(1);
                    }
                    flash_rom(h, patched, force);
                    eprintln!("Patched ROM written to the cartridge.");
                }
            }

            if ignore_checksum {
                eprintln!("Warning: skipped checksum verification.");
            }
        }

        Commands::Upgrade {
            from,
            output,
            check,
            acknowledge_incompatible_save,
            ignore_checksum,
            force,
            yes,
        } => do_upgrade(
            from.as_deref(),
            output.as_deref(),
            check,
            yes,
            acknowledge_incompatible_save,
            ignore_checksum,
            force,
        ),

        Commands::ReadPhotos { output, from, framed, rom, scaling } => {
            // Acquire the 128 KB SRAM (and, for --framed, the camera ROM) from
            // either supplied files or the cartridge.
            let (save, rom_data): (Vec<u8>, Option<Vec<u8>>) = match from {
                Some(path) => {
                    let save = fs::read(&path).unwrap_or_else(|e| {
                        eprintln!("Error reading file: {e}");
                        process::exit(1);
                    });
                    let rom_data = if framed {
                        let rp = rom.unwrap_or_else(|| {
                            eprintln!("--framed with --from also needs --rom <camera-rom-file>.");
                            process::exit(1);
                        });
                        Some(fs::read(&rp).unwrap_or_else(|e| {
                            eprintln!("Error reading ROM file: {e}");
                            process::exit(1);
                        }))
                    } else {
                        None
                    };
                    (save, rom_data)
                }
                None => {
                    let mut device = open_device();
                    let info = read_cart_info(device.as_mut());
                    if info.mbc_type != cartridge::MBC_POCKET_CAMERA {
                        eprintln!(
                            "This is not a Game Boy Camera cartridge (MBC: {} 0x{:02X}).",
                            info.mbc_name(),
                            info.mbc_type
                        );
                        process::exit(1);
                    }
                    eprintln!("Reading Game Boy Camera SRAM ({})...", format_size(info.ram_size));
                    let save = device
                        .read_save(ChipType::Unknown, info.rom_size, info.ram_size, &|cur| {
                            print_progress("Reading save", cur, info.ram_size)
                        })
                        .unwrap_or_else(|e| {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        });
                    let rom_data = if framed {
                        match rom {
                            Some(rp) => Some(fs::read(&rp).unwrap_or_else(|e| {
                                eprintln!("Error reading ROM file: {e}");
                                process::exit(1);
                            })),
                            None => {
                                eprintln!("Reading Game Boy Camera ROM for frames ({})...", format_size(info.rom_size));
                                Some(
                                    device
                                        .read_rom(ChipType::Unknown, info.rom_size, 0, &|cur| {
                                            print_progress("Reading ROM", cur, info.rom_size)
                                        })
                                        .unwrap_or_else(|e| {
                                            eprintln!("\nError: {e}");
                                            process::exit(1);
                                        }),
                                )
                            }
                        }
                    } else {
                        None
                    };
                    (save, rom_data)
                }
            };

            let slots = cartridge::camera_photo_slots(&save);
            match slots.len() {
                0 => {
                    println!("Found no photos on your Game Boy Camera.");
                    return;
                }
                1 => println!("Found 1 photo on your Game Boy Camera."),
                n => println!("Found {n} photos on your Game Boy Camera."),
            }

            fs::create_dir_all(&output).unwrap_or_else(|e| {
                eprintln!("Error creating output directory: {e}");
                process::exit(1);
            });

            let (w, h) = if framed {
                (cartridge::CAMERA_FRAME_WIDTH as u32, cartridge::CAMERA_FRAME_HEIGHT as u32)
            } else {
                (cartridge::CAMERA_PHOTO_WIDTH as u32, cartridge::CAMERA_PHOTO_HEIGHT as u32)
            };

            for (i, &slot) in slots.iter().enumerate() {
                let pixels = if framed {
                    cartridge::decode_camera_photo_framed(&save, rom_data.as_ref().unwrap(), slot)
                } else {
                    cartridge::decode_camera_photo(&save, slot)
                };
                let Some(pixels) = pixels else {
                    eprintln!("Photo {} (slot {slot}) is out of range; skipping.", i + 1);
                    continue;
                };
                let pixels = cartridge::scale_block(&pixels, w as usize, h as usize, scaling as usize);
                let path = output.join(format!("photo_{:02}.png", i + 1));
                if let Err(e) = write_gray_png(&path, w * scaling, h * scaling, &pixels) {
                    eprintln!("Error writing {}: {e}", path.display());
                    process::exit(1);
                }
            }
            eprintln!("Wrote {} photo(s) to {}.", slots.len(), output.display());
        }

        Commands::WritePhoto { image, from, output, slot, frame, no_dither, yes } => {
            let pixels = load_camera_image(&image, !no_dither).unwrap_or_else(|e| {
                eprintln!("Error reading image: {e}");
                process::exit(1);
            });

            if let (Some(o), Some(f)) = (output.as_deref(), from.as_deref())
                && o == f
            {
                eprintln!("Output is the same as the input; pass a different -o <path>.");
                process::exit(1);
            }

            // Open the device if the cart is the source or destination, and make sure
            // it's actually a Game Boy Camera before reading or writing its save.
            let mut handle = open_if_cart(from.as_deref(), output.as_deref());
            if let Some(h) = handle.as_ref()
                && h.info.mbc_type != cartridge::MBC_POCKET_CAMERA
            {
                eprintln!(
                    "This is not a Game Boy Camera cartridge (MBC: {} 0x{:02X}).",
                    h.info.mbc_name(),
                    h.info.mbc_type
                );
                process::exit(1);
            }

            // Source: the supplied save file, or the camera's SRAM.
            let mut save = match from.as_deref() {
                Some(base) => fs::read(base).unwrap_or_else(|e| {
                    eprintln!("Error reading save: {e}");
                    process::exit(1);
                }),
                None => {
                    let h = handle.as_mut().expect("cart handle is opened when reading the cart");
                    let (rom_size, ram_size) = (h.info.rom_size, h.info.ram_size);
                    eprintln!("Reading camera save ({})...", format_size(ram_size));
                    h.device
                        .read_save(ChipType::Unknown, rom_size, ram_size, &|cur| {
                            print_progress("Reading", cur, ram_size)
                        })
                        .unwrap_or_else(|e| {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        })
                }
            };

            let slot = pick_camera_slot(&save, slot);
            cartridge::inject_camera_photo(&mut save, slot, &pixels, frame).unwrap_or_else(|e| {
                eprintln!("Error injecting photo: {e}");
                process::exit(1);
            });

            // Destination: a save file, or back onto the camera.
            match output.as_deref() {
                Some(out) => {
                    fs::write(out, &save).unwrap_or_else(|e| {
                        eprintln!("Error writing output: {e}");
                        process::exit(1);
                    });
                    eprintln!("Injected {} into slot {slot}; wrote {}.", image.display(), out.display());
                }
                None => {
                    let h = handle.as_mut().expect("cart handle is opened when writing the cart");
                    if !yes
                        && !confirm(&format!(
                            "Write {} into slot {slot} on the camera? This overwrites its save.",
                            image.display()
                        ))
                    {
                        eprintln!("Aborted.");
                        process::exit(1);
                    }
                    let rom_size = h.info.rom_size;
                    eprintln!("Writing camera save ({})...", format_size(save.len() as u32));
                    h.device
                        .write_save(ChipType::Unknown, rom_size, &save, &|cur| {
                            print_progress("Writing", cur, save.len() as u32)
                        })
                        .unwrap_or_else(|e| {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        });
                    eprintln!("Injected {} into slot {slot} on the camera.", image.display());
                }
            }
        }

        Commands::ReadRtc { output } => {
            let mut device = open_device();
            let info = read_cart_info(device.as_mut());
            require_rtc(&info);

            match device.read_rtc(info.rom_size, info.ram_size) {
                Ok(payload) => {
                    match cartridge::RtcData::parse(&payload) {
                        Some(rtc) => {
                            println!("RTC: {rtc}");
                            if !rtc.is_valid() {
                                eprintln!(
                                    "Warning: RTC values are out of range — the cartridge \
                                     battery is likely dead."
                                );
                            }
                        }
                        None => eprintln!("Could not parse RTC payload ({} bytes).", payload.len()),
                    }
                    if let Some(path) = output {
                        fs::write(&path, &payload).unwrap_or_else(|e| {
                            eprintln!("Error writing file: {e}");
                            process::exit(1);
                        });
                        eprintln!("Wrote raw RTC ({} bytes) to {}.", payload.len(), path.display());
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    process::exit(1);
                }
            }
        }

        Commands::WriteRtc { from, days, hours, minutes, seconds, yes } => {
            let mut device = open_device();
            let info = read_cart_info(device.as_mut());
            require_rtc(&info);

            let payload = match from {
                Some(path) => {
                    let data = fs::read(&path).unwrap_or_else(|e| {
                        eprintln!("Error reading file: {e}");
                        process::exit(1);
                    });
                    if data.len() != 40 {
                        eprintln!("RTC backup must be exactly 40 bytes (got {}).", data.len());
                        process::exit(1);
                    }
                    data
                }
                None => {
                    if seconds > 59 || minutes > 59 || hours > 23 {
                        eprintln!("Out-of-range time (max 23:59:59).");
                        process::exit(1);
                    }
                    cartridge::RtcData {
                        seconds,
                        minutes,
                        hours,
                        days,
                        halt: false,
                        day_carry: false,
                    }
                    .to_payload()
                }
            };

            if !yes && !confirm("Set the real-time clock on the cartridge?") {
                eprintln!("Aborted.");
                process::exit(1);
            }

            eprintln!("Writing RTC...");
            match device.write_rtc(info.rom_size, info.ram_size, &payload) {
                Ok(()) => {
                    eprintln!("RTC written.");
                    // Read back to confirm it took (a dead battery won't persist it).
                    if let Ok(rb) = device.read_rtc(info.rom_size, info.ram_size)
                        && let Some(rtc) = cartridge::RtcData::parse(&rb) {
                            eprintln!("Read back: {rtc}");
                            if !rtc.is_valid() {
                                eprintln!(
                                    "Note: read-back is out of range — a dead RTC battery \
                                     won't hold the written values."
                                );
                            }
                        }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    process::exit(1);
                }
            }
        }
    }
}

/// RTC trailer lengths we accept on restore: 40 = throwback's own register-only block,
/// 44 = old VBA (32-bit timestamp), 48 = VBA-M/mGBA (64-bit timestamp). We always
/// *write* the 48-byte form (`cartridge.rs` register payload + a 64-bit unix timestamp).
const RTC_TRAILER_SIZES: [usize; 3] = [40, 44, 48];
const RTC_REGS_LEN: usize = 40;

/// Append the 48-byte emulator RTC trailer to a GB save: the 40-byte register payload
/// (current + latched, as the device returns it) plus a 64-bit little-endian unix
/// timestamp, matching the VBA-M/mGBA `.sav` format.
fn append_rtc_trailer(save: &mut Vec<u8>, payload: &[u8]) {
    save.extend_from_slice(&payload[..RTC_REGS_LEN]);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    save.extend_from_slice(&ts.to_le_bytes());
}

/// Exit with a clear message unless the cartridge is a GB cart with an RTC.
fn require_rtc(info: &CartridgeInfo) {
    if info.cart_type != CartridgeType::GB {
        eprintln!("RTC is only available on Game Boy cartridges.");
        process::exit(1);
    }
    if !info.has_rtc() {
        eprintln!("This cartridge has no real-time clock (MBC: {}).", info.mbc_name());
        process::exit(1);
    }
}
