# Reader BT Developer Documentation
**Version 1.5 (2026-05-27)**

This document describes the Bluetooth® Low Energy (BLE) interface to [SPORTident Reader BT](/products/stations/reader-bt). It specifies services, characteristics, wire format, and the application-level request/response messages that are sent over the characteristics.

You can find the source code of a sample application on [GitHub](https://github.com/sportidentgmbh/developers/).

### Terminology
* **Reader**: SPORTident Reader BT device
* **Client**: The mobile/desktop app connecting to the reader
* **User**: The person operating the client app, who may interact with the reader by inserting/removing cards, configuring settings, etc.
* **SI-Card**: SPORTident Card inserted into the reader
* **Service/Characteristic UUIDs**: Defined for the SPORTident Reader BT
* **Message**: An application payload written to / notified from a characteristic

---

## Bluetooth Low Energy

BLE GATT (Generic Attribute Profile) defines how data is structured and exchanged between a BLE client and server.

* A **service** groups related functionality (for example, device settings or card readout).
* A **characteristic** inside a service contains a value and access properties (read, write, notify).
* The client discovers services and characteristics after connection, then subscribes to notifications where required.
* For Reader BT, requests are typically written by the client and responses are returned as notifications on the related characteristic.

In short: GATT provides the application-level data model on top of BLE, while this document specifies the exact UUIDs, message structure, and payload formats used by Reader BT.

---

## Device Discovery

The reader must be in advertising mode, which is activated with the “Service/OFF” instruction card.

The client then needs to scan for peripherals advertising the **primary service UUID**:
* **Primary service UUID used for scanning**: `bd510001-6aec-4628-a146-f3e95bc49e62`

> **Notes on Discovery:**
> * The reader advertises the primary service UUID but not the card readout or backup service UUIDs, so scanning should target the primary service.
> * BLE scanning is 20 seconds, which matches the time the reader remains in advertising mode.
> * Depending on the platform/OS, additional filtering by device name prefix (e.g., “Reader BT”) may be possible and can help to reduce noise from non-reader devices.

---

## Connecting

Connect to a discovered (or known) device by its BLE device ID (MAC address). 

Depending on the BLE library and operating system used, a maximum MTU length (517) may be required. After connecting, the client may validate that required services exist by enumerating services and checking UUIDs.

---

## Services and Characteristics

### Overview
All UUIDs below are 128-bit UUIDs. Reader BT UUIDs start with the 32-bit identifier `bd5100xx`, where `xx` specifies the GATT service or characteristic.

Generally and unless stated otherwise: The client writes requests to a characteristic. The reader replies via a notification on the same characteristic (or a service-specific notify characteristic). All client -> reader writes must be performed as **Write With Response** (GATT-level write with an acknowledgment).

A client should subscribe to the relevant services/characteristics:
* **Device settings and infos**: Subscribe to the *Read settings* characteristic.
* **Card data readout**: Subscribe to the *Card state* and *Card data* characteristics.
* **Read data from the device memory**: Subscribe to the *Sessions* characteristic.

### Settings Service
Service for configuring device settings and requesting device information.
* **Service UUID**: `bd510001-6aec-4628-a146-f3e95bc49e62`

| Characteristic Name | UUID | Properties | Direction | Description / Used For |
| :--- | :--- | :--- | :--- | :--- |
| **Read settings** | `bd510002-6aec-4628-a146-f3e95bc49e62` | Notify, Write With Response | Bidirectional | Reading device info, code number, and other settings. |
| **Write settings** | `bd510003-6aec-4628-a146-f3e95bc49e62` | Write With Response, Write Without Response | Client -> Reader | Writing station configuration such as code number, sleep mode, etc. |

### Card Readout Service
Service for reading data from an SI-Card.
* **Service UUID**: `bd510011-6aec-4628-a146-f3e95bc49e62`

| Characteristic Name | UUID | Properties | Direction | Description / Used For |
| :--- | :--- | :--- | :--- | :--- |
| **Card state** | `bd510012-6aec-4628-a146-f3e95bc49e62` | Notify | Reader -> Client | Card insert/remove state changes. If the station is in auto-readout mode, an insert notification triggers automatic readout. |
| **Card data** | `bd510013-6aec-4628-a146-f3e95bc49e62` | Notify, Write With Response | Bidirectional | Readout requests, readout data (possibly segmented), and feedback commands. |

### Sessions Service
A service for reading data from the device’s internal memory.
* **Service UUID**: `bd510031-6aec-4628-a146-f3e95bc49e62`

| Characteristic Name | UUID | Properties | Direction | Description / Used For |
| :--- | :--- | :--- | :--- | :--- |
| **Sessions** | `bd510032-6aec-4628-a146-f3e95bc49e62` | Notify, Write With Response | Bidirectional | Session lookup table requests/responses and session data upload. |

---

## Wire Format

### Message Structure
All multi-byte integers are encoded as **little-endian**. Every message begins with a 4-byte header.

| Offset | Size (Bytes) | Type | Name | Description |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 2 | `uint16` | `message_id` | Message ID identifying the type of message |
| 2 | 2 | `uint16` | `payload_length` | Number of payload bytes following the header |
| 4 | N | `byte[]` | `payload` | Payload bytes, length defined by `payload_length` |

### Message Segmentation
The BLE transport may deliver large payloads (notably card readout and backup transfers) split across multiple notifications. The client must support reassembly via a dedicated message called the **wrapper message** (Message ID `0xA101`). The wrapper's payload contains a segment of the original unsegmented message.

**Wrapper message payload header:**
| Offset | Size (Bytes) | Type | Name | Description |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 1 | `uint8` | `packet_flag` | `0x01` = First packet<br>`0x00` = Subsequent packet<br>`0x02` = Last packet (`SegmentMarker`) |

* **If `packet_flag == 0x01` (First packet):**
  | Offset | Size (Bytes) | Type | Name | Description |
  | :--- | :--- | :--- | :--- | :--- |
  | 1 | 4 | `uint32` | `total_message_length` | Expected length (bytes) of the fully reassembled unsegmented message |
  | 5 | N | `byte[]` | `segment` | First segment data bytes |

* **If `packet_flag == 0x00` (Continuation) or `0x02` (Last):**
  | Offset | Size (Bytes) | Type | Name | Description |
  | :--- | :--- | :--- | :--- | :--- |
  | 1 | N | `byte[]` | `segment` | Segment data bytes |

**Reassembly Rules:**
1. On `First` packet (`0x01`): Clear the receiving buffer, store `total_message_length`, and append `segment`.
2. On `Continuation` packet (`0x00`): Append `segment` to the buffer.
3. On `Last` packet (`0x02`): Append `segment`. The total buffer length **must** now equal `total_message_length`.
4. After successful reassembly, treat the accumulated bytes as a **normal unsegmented message** (starting with its own 4-byte base header).

---

## Messages

### Overview
This section defines payload layouts following the 4-byte base header.

| Name | Message ID | Service / Characteristic | Direction | Notes |
| :--- | :--- | :--- | :--- | :--- |
| **CardStateChange** | `0x1101` | Card Readout / Card state | Reader -> Client | Card insert/remove events |
| **CardDataReadoutMinimalData** | `0x1102` | Card Readout / Card data | Client -> Reader / Reader -> Client | Minimal card readout request/response |
| **CardDataReadoutCompleteData** | `0x1103` | Card Readout / Card data | Client -> Reader / Reader -> Client | Complete card readout request/response |
| **Feedback** | `0x1110` | Card Readout / Card data | Client -> Reader | Trigger station feedback (LED/Buzzer) |
| **ReadDeviceInfo** | `0x010E` | Settings / Read settings | Client -> Reader / Reader -> Client | Request/receive hardware & software device info |
| **ReadAllSettings** | `0x010C` | Settings / Read settings | Client -> Reader / Reader -> Client | Request/receive all station settings |
| **WriteAllSettings** | `0x010D` | Settings / Write settings | Client -> Reader | Write all station settings |
| **SetToSleepMode** | `0x0101` | Settings / Write settings | Client -> Reader | Put the station into sleep mode |
| **ReadAvailableSessionsRequest** | `0x3101` | Sessions / Sessions | Client -> Reader | Request session lookup table page |
| **ReadAvailableSessionsResponse** | `0x3103` | Sessions / Sessions | Reader -> Client | Response containing session lookup table page |
| **ReadSessionDataRequest** | `0x3102` | Sessions / Sessions | Client -> Reader | Request session data by session number |
| **ReadSessionDataResponse** | `0x3104` | Sessions / Sessions | Reader -> Client | Response with session data (may be segmented) |

### Card state change
* **Message ID**: `0x1101`
* **Characteristic**: Notify on *Card state (bd510012)*

**Payload Layout (7 bytes):**
| Offset | Size (Bytes) | Type | Name | Notes |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 4 | `uint32` | `card_number` | SPORTident card number |
| 4 | 1 | `uint8` | `state` | `0` = Card Out, `1` = Card In |
| 5 | 2 | `uint16` | `code_number` | Control code number of the station |

### Feedback
The application can inform the user about the success of reading data. The Reader BT supports 2 feedback types. Feedback commands do not depend on whether an SI-Card is actively inside the device.

* **Message ID**: `0x1110`
* **Characteristic**: Write to *Card data (bd510013)*

**Payload Layout (1 byte):**
| Offset | Size (Bytes) | Type | Name | Description / Values |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 1 | `uint8` | `feedback_type` | `1` = `Feedback_Ok` (short flash/beep)<br>`2` = `Feedback_Error` (longer flash/beep pattern) |

*Note: If AutoReadout is activated on the device, the device will automatically emit feedback upon a successful punch, making manual application feedback unnecessary.*

### Read device info
Requests production metrics and unique hardware properties. To trigger this request, write a base 4-byte header containing `0x010E` and a `payload_length` of `0` to the characteristic.

* **Request Message ID**: `0x010E` (Write to *Read settings*)
* **Response Message ID**: `0x010E` (Notify on *Read settings*)

**Response Payload Layout:**
| Offset | Size (Bytes) | Type | Name | Description / Notes |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 4 | `uint32` | `serial_number` | Device serial number |
| 4 | 2 | `uint16` | `device_type` | Device type identifier (e.g., Reader BT is `0x00A7` / 167) |
| 6 | 2 | `uint8[2]` | `hw_version` | Hardware version array: `{ major, minor }` |
| 8 | 4 | `struct` | `production_date` | Custom structure: `{ year: uint16, month: uint8, day: uint8 }` |
| 12 | 4 | `struct` | `battery_date` | Custom structure: `{ year: uint16, month: uint8, day: uint8 }` |
| 16 | Variable | `byte[]` | `sw_version_ascii` | ASCII string containing the software version, NUL-terminated/padded |

### Read all settings
Requests the internal operating configuration of the Reader BT. Triggered by writing a 4-byte base header with `0x010C` and a `payload_length` of `0`.

* **Request Message ID**: `0x010C` (Write to *Read settings*)
* **Response Message ID**: `0x010C` (Notify on *Read settings*)

**Response Payload Layout (10 bytes):**
| Offset | Size (Bytes) | Type | Name | Description / Notes |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 2 | `uint16` | `code_number` | Station control number |
| 2 | 1 | `uint8` | `auto_read` | Boolean flag. `1` = Auto Readout is enabled on the device |
| 3 | 7 | `byte[]` | `reserved` | Reserved for future use |

### Write all settings
* **Message ID**: `0x010D`
* **Characteristic**: Write to *Write settings (bd510003)*

**Details:**
* Payload encoding matches the structure and alignment of the **Read all settings** response payload layout exactly (10 bytes total).
* Must use **Write With Response** at the GATT transport level.

### Read available sessions request
The internal memory of the Reader BT is segmented into chunks called sessions. A new session is generated automatically every time a client creates a connection. The last session in the sequence represents the active current session.

The internal registry behaves like a circular buffer capable of indexing a maximum of 291 sessions before older records get overwritten. The registry index is structured into pages containing a maximum of 5 records per page. Page 0 always targets the newest records.

* **Message ID**: `0x3101`
* **Characteristic**: Write to *Sessions (bd510032)*

**Payload Layout (1 byte):**
| Offset | Size (Bytes) | Type | Name | Description |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 1 | `uint8` | `page_number` | 0-based index of the page requested |

### Read available sessions response
* **Message ID**: `0x3103`
* **Characteristic**: Notify on *Sessions (bd510032)*

**Payload Layout (Header + Repeating Blocks):**
| Offset | Size (Bytes) | Type | Name | Description |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 1 | `uint8` | `page_number` | Page index being returned |
| 1 | 1 | `uint8` | `total_pages` | Total number of available pages inside the registry index |
| 2 | 1 | `uint8` | `entries_count` | Number of session lookup records following this header (max 5) |

Following the 3-byte header, a sequence of `entries_count` blocks of **Session Lookup Entries** (20 bytes each) is appended:

| Offset (Entry) | Size (Bytes) | Type | Name | Description / Notes |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 2 | `uint16` | `session_number` | Unique identifier for the indexed session |
| 2 | 8 | `uint64` | `start_time` | Unix time in seconds (Defaults to `0` if unknown) |
| 10 | 8 | `uint64` | `end_time` | Unix time in seconds (Or duration in seconds if `start_time == 0`) |
| 18 | 2 | `uint16` | `number_of_readouts` | Total count of card punch extractions stored within this session |

*Note: Current production readers do not feature an embedded Real-Time Clock (RTC), so `start_time` and `end_time` fields are regularly returned as `0`.*

### Read session data request
* **Message ID**: `0x3102`
* **Characteristic**: Write to *Sessions (bd510032)*

**Payload Layout (2 bytes):**
| Offset | Size (Bytes) | Type | Name | Description |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 2 | `uint16` | `session_number` | Target session ID extracted from a session lookup entry |

### Read session data response
* **Message ID**: `0x3104`
* **Characteristic**: Notify on *Sessions (bd510032)* (Highly prone to message segmentation)

**Payload Layout Header (8 bytes):**
| Offset | Size (Bytes) | Type | Name | Description |
| :--- | :--- | :--- | :--- | :--- |
| 0 | 2 | `uint16` | `session_number` | Session number identifier |
| 2 | 2 | `uint16` | `number_of_readouts` | Total count of historical readouts embedded within this payload |
| 4 | 4 | `uint32` | `session_data_length` | Total byte size of the raw session stream trailing this header |

The header is immediately followed by a raw contiguous sequence containing `number_of_readouts` independent **embedded card readout messages**. Each embedded entry is packed as:
1. **4-byte base header** (`message_id` + `payload_length`)
2. **`payload_length` bytes** containing the card readout payload data.

*Note: The `message_id` inside each nested card block will match either `0x1102` (Minimal Data Format) or `0x1103` (Complete Data Format).*

Recommended client workflow

1. Scan for devices advertising the Reader BT’s Settings service UUID or device name, starting with “Reader BT”
2. Connect to the selected device
3. Subscribe to needed services
   * Settings for configuration/info
   * Card readout for live readout
   * Sessions for backup memory session download
4. (Optional) Request device info, code number, and other settings
5. For live readout
   * Wait for CardStateChange notification then request CardDataReadout
   * Optionally trigger feedback after successful CardDataReadout event
6. For backup session download
   * Request lookup table page(s), then request session data

