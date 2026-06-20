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
    },
    /// Write save data from a file
    WriteSave {
        /// Input file path
        input: PathBuf,
    },
    /// Write ROM to a flash cart
    WriteRom {
        /// Input file path
        input: PathBuf,
        /// Write even if the cartridge isn't detected as a writeable flashcart
        #[arg(long)]
        force: bool,
    },
    /// Apply an IPS, UPS, or BPS patch to a ROM file
    ApplyPatch {
        /// Source ROM file
        rom: PathBuf,
        /// IPS patch file
        patch: PathBuf,
        /// Output patched ROM file
        #[arg(short, long)]
        output: PathBuf,
        /// Write even if the patched ROM fails header-checksum validation
        #[arg(long)]
        ignore_checksum: bool,
    },
    /// Check update services for a newer version of a ROM (or the inserted cart) and apply it
    Upgrade {
        /// ROM file to upgrade; omit to read and upgrade the inserted cartridge
        rom: Option<PathBuf>,
        /// Output ROM path (defaults to "<title> <version>.<ext>" next to the input)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Report available updates without applying or flashing anything
        #[arg(long)]
        check: bool,
        /// Cart mode: flash without the interactive confirmation
        #[arg(long)]
        write: bool,
        /// Proceed past a save-incompatible update without the interactive prompt
        #[arg(long)]
        acknowledge_incompatible_save: bool,
        /// Skip verification of the upgraded ROM
        #[arg(long)]
        ignore_checksum: bool,
        /// Cart mode: flash even if the cart isn't a detected writeable flashcart
        #[arg(long)]
        force: bool,
    },
    /// Extract photos from a Game Boy Camera cartridge (or a camera .sav)
    ReadCamera {
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
    /// Read the cartridge's real-time clock (MBC3 carts)
    ReadRtc {
        /// Optional file to save the raw 40-byte RTC payload as a backup
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Write the cartridge's real-time clock (MBC3 carts)
    WriteRtc {
        /// Restore from a raw 40-byte .rtc backup (takes precedence over the flags)
        #[arg(long)]
        input: Option<PathBuf>,
        #[arg(long, default_value_t = 0)]
        days: u16,
        #[arg(long, default_value_t = 0)]
        hours: u8,
        #[arg(long, default_value_t = 0)]
        minutes: u8,
        #[arg(long, default_value_t = 0)]
        seconds: u8,
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

fn apply_patch(rom_path: &PathBuf, patch_path: &PathBuf, output: &PathBuf, ignore_checksum: bool) {
    let rom = fs::read(rom_path).unwrap_or_else(|e| {
        eprintln!("Error reading ROM: {e}");
        process::exit(1);
    });

    let patch_data = fs::read(patch_path).unwrap_or_else(|e| {
        eprintln!("Error reading patch: {e}");
        process::exit(1);
    });

    let patch = Patch::load(&patch_data).unwrap_or_else(|e| {
        eprintln!("Error parsing patch: {e}");
        process::exit(1);
    });

    let format = patch.format_name();
    let original_len = rom.len();
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
        // verification skipped entirely below
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

    fs::write(output, &patched).unwrap_or_else(|e| {
        eprintln!("Error writing output: {e}");
        process::exit(1);
    });

    eprintln!("Applied {} to {}", patch_path.display(), rom_path.display());
    eprintln!(
        "Wrote {} ({} -> {} bytes)",
        output.display(),
        format_size(original_len as u32),
        format_size(patched.len() as u32)
    );

    if ignore_checksum {
        eprintln!("Warning: skipped checksum verification.");
    }
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

/// Make a string safe to use as a filename.
fn sanitize_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || " ._-()".contains(c) { c } else { '_' })
        .collect();
    let s = s.trim();
    if s.is_empty() { "game".to_string() } else { s.to_string() }
}

/// Default output path for file mode: "<title> <version>.<ext>" next to the input.
fn default_output_name(input: &Path, id: &Identity, up: &Upgrade) -> PathBuf {
    let ext = input.extension().and_then(|e| e.to_str()).unwrap_or("gbc");
    let name = format!("{} {}.{ext}", sanitize_filename(&id.title), up.to_version);
    match input.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => PathBuf::from(name),
    }
}

fn upgrade_file(path: &Path, output: Option<&Path>, check: bool, ignore_checksum: bool) {
    let rom = fs::read(path).unwrap_or_else(|e| {
        eprintln!("Error reading ROM: {e}");
        process::exit(1);
    });

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

    let out_path = match output {
        Some(p) => p.to_path_buf(),
        None => default_output_name(path, &id, &up),
    };
    if out_path == path {
        eprintln!("Refusing to overwrite the input ROM; pass -o <path> for the output.");
        process::exit(1);
    }

    fs::write(&out_path, &upgraded).unwrap_or_else(|e| {
        eprintln!("Error writing output: {e}");
        process::exit(1);
    });
    println!(
        "Upgraded {} {} -> {}; wrote {}",
        id.title,
        up.from_version,
        up.to_version,
        out_path.display()
    );
    if ignore_checksum {
        eprintln!("Warning: skipped verification.");
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

#[allow(clippy::fn_params_excessive_bools)]
fn upgrade_cart(
    output: Option<&Path>,
    check: bool,
    write: bool,
    acknowledge_incompatible_save: bool,
    ignore_checksum: bool,
    force: bool,
) {
    let mut device = open_device();
    let info = read_cart_info(device.as_mut());
    let rom = dump_cart_rom(device.as_mut(), &info);

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

    // Keep a copy on disk if requested (written before flashing, so it survives even
    // if the flash fails).
    if let Some(out) = output {
        fs::write(out, &upgraded).unwrap_or_else(|e| {
            eprintln!("Error writing output: {e}");
            process::exit(1);
        });
        eprintln!("Wrote {}", out.display());
    }

    // Save-incompatibility acknowledgement (only matters if the cart holds a save).
    if up.save_compatible == Some(false) && info.ram_size > 0 && !acknowledge_incompatible_save {
        eprintln!(
            "This update is NOT save-compatible; the save on your cartridge may not carry over."
        );
        eprintln!("Back up the save first with `throwback read-save` if you want to keep it.");
        if !confirm("Continue and flash anyway?") {
            eprintln!("Aborted.");
            process::exit(1);
        }
    }

    // Flash confirmation.
    if !write
        && !confirm(&format!(
            "Flash {} {} to the cartridge? This erases it.",
            id.title, up.to_version
        ))
    {
        eprintln!("Aborted.");
        process::exit(1);
    }

    // Flashcart-writeable guard (same as write-rom).
    if !force {
        match device.detect_flashcart() {
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

    // Pad to a 64-byte boundary and flash.
    let mut padded = upgraded;
    if !padded.len().is_multiple_of(64) {
        padded.resize(padded.len() + (64 - padded.len() % 64), 0xFF);
    }
    eprintln!("Writing {} to the cartridge...", format_size(padded.len() as u32));
    match device.write_rom(
        &padded,
        info.ram_size,
        &|cur| print_progress("Writing", cur, padded.len() as u32),
        &|msg| eprintln!("\r{msg}    "),
    ) {
        Ok(()) => eprintln!("Upgraded {} to {} on the cartridge.", id.title, up.to_version),
        Err(e) => {
            eprintln!("\nError: {e}");
            process::exit(1);
        }
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
            eprintln!("Saved to {}", output.display());
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
            eprintln!("Saved to {}", output.display());
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
    eprintln!("Saved to {}", output.display());
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

        Commands::ReadSave { output } => {
            let mut device = open_device();
            let info = read_cart_info(device.as_mut());

            match info.cart_type {
                CartridgeType::GB => {
                    if info.ram_size == 0 {
                        eprintln!("This cartridge has no save RAM.");
                        process::exit(1);
                    }

                    eprintln!("Reading save ({})...", format_size(info.ram_size));

                    match device.read_save(
                        ChipType::Unknown,
                        info.rom_size,
                        info.ram_size,
                        &|cur| print_progress("Reading", cur, info.ram_size),
                    ) {
                        Ok(save) => {
                            fs::write(&output, &save).unwrap_or_else(|e| {
                                eprintln!("Error writing file: {e}");
                                process::exit(1);
                            });
                            eprintln!("Saved to {}", output.display());
                        }
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
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
                            eprintln!("Saved to {}", output.display());
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
                            eprintln!("Saved to {}", output.display());
                        }
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::WriteSave { input } => {
            let mut device = open_device();
            let info = read_cart_info(device.as_mut());

            let data = fs::read(&input).unwrap_or_else(|e| {
                eprintln!("Error reading file: {e}");
                process::exit(1);
            });

            match info.cart_type {
                CartridgeType::GB => {
                    if info.ram_size == 0 {
                        eprintln!("This cartridge has no save RAM.");
                        process::exit(1);
                    }

                    if data.len() != info.ram_size as usize {
                        eprintln!(
                            "Warning: file size ({}) doesn't match cartridge RAM size ({}).",
                            format_size(data.len() as u32),
                            format_size(info.ram_size)
                        );
                    }

                    eprintln!("Writing save ({})...", format_size(data.len() as u32));

                    match device.write_save(ChipType::Unknown, info.rom_size, &data, &|cur| {
                        print_progress("Writing", cur, data.len() as u32);
                    }) {
                        Ok(()) => eprintln!("Save written successfully."),
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
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
                        Ok(()) => eprintln!("Save written successfully."),
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
                        Ok(()) => eprintln!("Save written successfully."),
                        Err(e) => {
                            eprintln!("\nError: {e}");
                            process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::WriteRom { input, force } => {
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
                    eprintln!("ROM written successfully.");
                }
                Err(e) => {
                    eprintln!("\nError: {e}");
                    process::exit(1);
                }
            }
        }

        Commands::ApplyPatch { rom, patch, output, ignore_checksum } => {
            apply_patch(&rom, &patch, &output, ignore_checksum);
        }

        Commands::Upgrade {
            rom,
            output,
            check,
            write,
            acknowledge_incompatible_save,
            ignore_checksum,
            force,
        } => match rom {
            Some(path) => upgrade_file(&path, output.as_deref(), check, ignore_checksum),
            None => upgrade_cart(
                output.as_deref(),
                check,
                write,
                acknowledge_incompatible_save,
                ignore_checksum,
                force,
            ),
        },

        Commands::ReadCamera { output, from, framed, rom, scaling } => {
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
            eprintln!("Saved {} photo(s) to {}", slots.len(), output.display());
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
                        eprintln!("Saved raw RTC ({} bytes) to {}", payload.len(), path.display());
                    }
                }
                Err(e) => {
                    eprintln!("Error: {e}");
                    process::exit(1);
                }
            }
        }

        Commands::WriteRtc { input, days, hours, minutes, seconds } => {
            let mut device = open_device();
            let info = read_cart_info(device.as_mut());
            require_rtc(&info);

            let payload = match input {
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
