# Epilogue GB Operator USB Protocol

Reverse-engineered from the Playback v1.9.0 macOS binary and community research ([jaames/gb-operator-reverse-engineering](https://github.com/jaames/gb-operator-reverse-engineering), [N0ciple/gbopyrator](https://github.com/N0ciple/gbopyrator)).

## USB Device

| Field | Value |
|-------|-------|
| Vendor ID | `0x16D0` (firmware 9+) or `0x1D50` (older) |
| Product IDs | `0x123B`, `0x123C`, `0x123D` (firmware 9+) or `0x6018` (older) |
| Device Class | CDC ACM (Communications Device Class, Abstract Control Model) |

### Interfaces

The device exposes 4 USB interfaces:

| Interface | Class | Description | Endpoints |
|-----------|-------|-------------|-----------|
| 0 | CDC Control (2/2) | Legacy protocol control | EP 0x82 (Interrupt IN) |
| 1 | CDC Data (10/0) | Legacy protocol data | EP 0x01 (Bulk OUT), EP 0x81 (Bulk IN) |
| 2 | CDC Control (2/6) | Streaming protocol control | EP 0x84 (Interrupt IN) |
| 3 | CDC Data (10/0) | Streaming protocol data | EP 0x03 (Bulk OUT), EP 0x83 (Bulk IN) |

The legacy protocol (interfaces 0+1) is used for communication. The streaming protocol (interfaces 2+3) exists on newer firmware but is not required.

### Initialization

Before the device will respond to commands, the host must:

1. Claim interface 0 (CDC control) and interface 1 (CDC data)
2. Send `SET_LINE_CODING` (request `0x20`, type `0x21`) with 115200/8N1:
   ```
   00 C2 01 00  (115200 baud, LE)
   00           (1 stop bit)
   00           (no parity)
   08           (8 data bits)
   ```
3. Send `SET_CONTROL_LINE_STATE` (request `0x22`, type `0x21`, value `0x0003`) to activate DTR+RTS

## Packet Format

All command packets are **64 bytes** with the following layout:

```
Offset  Size  Field
0       1     Command byte
1       1     Chip type (for save operations)
2       4     ROM size (u32, little-endian)
6       4     Save/RAM size (u32, little-endian)
10      50    Zero padding
60      4     CRC-32/MPEG-2 checksum of bytes 0-59 (little-endian)
```

### CRC-32/MPEG-2

The checksum uses polynomial `0x04C11DB7` with initial value `0xFFFFFFFF` and **no** final XOR inversion. This differs from standard CRC-32 which inverts the final value.

## Commands

| Byte | Name | Description |
|------|------|-------------|
| `0x00` | ReadGame | Dump cartridge ROM |
| `0x01` | WriteGame | Write ROM to flash cart |
| `0x02` | ReadSave | Read save/SRAM data |
| `0x03` | WriteSave | Write save/SRAM data |
| `0x04` | ReadSignature | Read cartridge info |
| `0x05` | RebootSysDfu | Enter system DFU mode |
| `0x08` | DisableLed | Turn off device LED |
| `0x09` | ReadRTC | Read real-time clock |
| `0x10` | WriteRTC | Write real-time clock |
| `0x11` | StartPeripheral | Start peripheral mode (e.g. Game Boy Camera) |
| `0x12` | UsePeripheral | Interact with peripheral |
| `0x13` | StopPeripheral | Stop peripheral mode |
| `0x20` | FactoryReset | Factory reset device |
| `0x35` | RebootAppDfu | Enter app DFU mode |

## Chip Types

The chip type byte (offset 1) selects the save memory type for save operations:

| Value | Type | Typical Use |
|-------|------|-------------|
| `0` | Unknown | GB/GBC (auto-detected by device) |
| `1` | EEPROM | GBA EEPROM saves (512B or 8KB) |
| `2` | SRAM | GB/GBA SRAM saves (typically 32KB) |
| `3` | FLASH | GBA Flash saves (64KB or 128KB) |

## Response Framing

The device uses a framing protocol with magic bytes `C0 DE`:

```
C0 DE 00 <cmd> FB  — Ready (command acknowledged, processing)
C0 DE 01 <cmd> FB  — Done (operation complete)
```

Between framing packets and data, the device sends **zero-padding packets** (typically ~7 full 64-byte zero packets) and occasional **zero-length packets** (ZLPs). The host must skip these when reading responses.

## ReadSignature (0x04)

Probes the currently inserted cartridge.

### Request
```
[0x04] [0x00] [0x00 × 4] [0x00 × 4] [0x00 × 50] [CRC × 4]
```

### Response Flow
1. Send command
2. Receive `C0 DE 00 04 FB` (ready)
3. Skip zero-padding packets
4. Receive 64-byte signature data
5. Receive `C0 DE 01 04 FB` (done)

### Signature Data

| Offset | Size | Field |
|--------|------|-------|
| 0x00 | 1 | Firmware version |
| 0x02 | 1 | Cart type: `0x20` = GB/GBC, `0x30` = GBA |
| 0x03 | 1 | Cartridge present flag 1 |
| 0x04 | 1 | Cartridge present flag 2 |
| 0x0D | 1 | First character of game title (ASCII) |
| 0x0E | 1 | MBC type (GB) or Game code byte 1 (GBA) |
| 0x0F | 1 | ROM size code (GB) or Game code byte 2 (GBA) |
| 0x10 | 1 | RAM size code (GB) or Game code byte 3 (GBA) |
| 0x11 | 1 | Header checksum (GB) or Region byte (GBA) |
| 0x12 | 2 | Global checksum (GB, little-endian) |
| 0x1A | 3 | Firmware version triplet (major.minor.patch) |

A cartridge is present when bytes 0x03 and 0x04 are not both zero.

### GB ROM/RAM Size Computation

ROM and RAM sizes are computed from the header codes, not reported directly:

**ROM size:** `32KB << rom_size_code`

| Code | Size |
|------|------|
| 0x00 | 32 KB |
| 0x01 | 64 KB |
| 0x02 | 128 KB |
| 0x03 | 256 KB |
| 0x04 | 512 KB |
| 0x05 | 1 MB |
| 0x06 | 2 MB |
| 0x07 | 4 MB |
| 0x08 | 8 MB |

**RAM size:**

| Code | Size |
|------|------|
| 0x00 | None |
| 0x01 | 2 KB |
| 0x02 | 8 KB |
| 0x03 | 32 KB |
| 0x04 | 128 KB |
| 0x05 | 64 KB |

### GBA Size Detection

The device does not report ROM or save sizes for GBA cartridges. The official Playback app uses an internal encrypted game database (VGDB) to look up sizes by game code.

An alternative approach is to dump the maximum ROM size (32 MB), then:
- **Trim ROM:** Find the last non-`0xFF` byte and round up to the nearest power of two
- **Detect save type:** Scan the ROM for GBA save library strings:
  - `EEPROM_V` → EEPROM (8 KB default)
  - `FLASH1M_V` → Flash 128 KB
  - `FLASH512_V` → Flash 64 KB
  - `FLASH_V` → Flash 64 KB
  - `SRAM_V` / `SRAM_F_V` → SRAM 32 KB

## ReadGame (0x00)

Dumps cartridge ROM data.

### Request
```
[0x00] [chip] [rom_size LE] [save_size LE] [0x00 × 50] [CRC × 4]
```

For GB/GBC, chip = 0 and save_size = ram_size. For GBA, chip = 0 and save_size = 0.

### Response Flow
1. Send command
2. Receive `C0 DE 00 00 FB` (ready)
3. Send 64-byte all-zeros ACK
4. Skip zero-padding packets until first data packet
5. Read 64-byte data packets continuously
6. Every 320 data packets: send 64-byte all-zeros ACK (flow control)
7. Receive `C0 DE 01 00 FB` (done)

## ReadSave (0x02)

Reads save/SRAM data from the cartridge.

### Request
```
[0x02] [chip] [rom_size LE] [save_size LE] [0x00 × 50] [CRC × 4]
```

### Response Flow
1. Send command
2. Receive `C0 DE 00 02 FB` (ready)
3. Skip zero-padding packets
4. Read data packets until `save_size` bytes received
5. Receive `C0 DE 01 02 FB` (done)

## WriteSave (0x03)

Writes save data to the cartridge.

### Request
```
[0x03] [chip] [rom_size LE] [save_size LE] [0x00 × 50] [CRC × 4]
```

### Response Flow
1. Send command
2. Receive `C0 DE 00 03 FB` (ready)
3. Send data in 64-byte chunks, with ~0.1ms delay between chunks
4. After each chunk: read acknowledgment from device
5. Receive `C0 DE 01 03 FB` (done)

## Epilogue Game ID

For GB/GBC, the Epilogue Game ID is constructed as:
```
<title_first_char uppercase> + <header_checksum as 2-char hex> + <global_checksum as 4-char hex>
```

For GBA:
```
<title_first_char> + <3-char game code>
```

## References

- [jaames/gb-operator-reverse-engineering](https://github.com/jaames/gb-operator-reverse-engineering) — Original protocol RE notes
- [N0ciple/gbopyrator](https://github.com/N0ciple/gbopyrator) — Python implementation (GB/GBC)
- [bozothegeek/gbopyrator GBA-TENTATIVES](https://github.com/bozothegeek/gbopyrator/tree/GBA-TENTATIVES) — GBA save read/write
- Playback v1.9.0 macOS binary — Disassembled for command enum, chip types, packet layout, and VGDB database usage
