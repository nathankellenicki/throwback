mod cartridge;
mod device;

use cartridge::{CartridgeInfo, CartridgeType, detect_eeprom_size, detect_gba_save, format_size, trim_gba_rom};
use clap::{Parser, Subcommand};
use device::{ChipType, Device};
use std::fs;
use std::path::PathBuf;
use std::process;

const GBA_MAX_ROM: u32 = 32 * 1024 * 1024; // 32 MB

#[derive(Parser)]
#[command(name = "flashback", about = "CLI for the Epilogue GB Operator")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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
    },
}

fn open_device() -> Device {
    match Device::open() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: {e}");
            process::exit(1);
        }
    }
}

fn read_cart_info(device: &mut Device) -> CartridgeInfo {
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

fn dump_rom_gb(device: &mut Device, info: &CartridgeInfo, output: &PathBuf) {
    eprintln!("Dumping GB ROM ({})...", format_size(info.rom_size));

    match device.read_rom(ChipType::Unknown, info.rom_size, info.ram_size, |cur| {
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

fn dump_rom_gba(device: &mut Device, _info: &CartridgeInfo, output: &PathBuf) {
    eprintln!(
        "Dumping GBA ROM (reading {} max, will auto-trim)...",
        format_size(GBA_MAX_ROM)
    );

    match device.read_rom(ChipType::Unknown, GBA_MAX_ROM, 0, |cur| {
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

fn main() {
    let cli = Cli::parse();

    match cli.command {
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

            let info = CartridgeInfo::from_bytes(&data);
            if !info.present {
                eprintln!("No cartridge inserted.");
                process::exit(1);
            }
            println!("{info}");
        }

        Commands::DumpRom { output } => {
            let mut device = open_device();
            let info = read_cart_info(&mut device);

            match info.cart_type {
                CartridgeType::GB => dump_rom_gb(&mut device, &info, &output),
                CartridgeType::GBA => dump_rom_gba(&mut device, &info, &output),
            }
        }

        Commands::ReadSave { output } => {
            let mut device = open_device();
            let info = read_cart_info(&mut device);

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
                        |cur| print_progress("Reading", cur, info.ram_size),
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

                    let rom = match device.read_rom(ChipType::Unknown, GBA_MAX_ROM, 0, |cur| {
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

                    match device.read_save(chip, rom_size, save_size, |cur| {
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
            }
        }

        Commands::WriteSave { input } => {
            let mut device = open_device();
            let info = read_cart_info(&mut device);

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

                    match device.write_save(ChipType::Unknown, info.rom_size, &data, |cur| {
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

                    let rom = match device.read_rom(ChipType::Unknown, GBA_MAX_ROM, 0, |cur| {
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

                    match device.write_save(chip, rom_size, &data, |cur| {
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

        Commands::WriteRom { input } => {
            let mut device = open_device();

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

            match device.write_rom(&padded, |cur| {
                print_progress("Writing", cur, padded.len() as u32);
            }, |msg| {
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
    }
}
