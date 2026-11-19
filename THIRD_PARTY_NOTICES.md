# Third-Party Notices and References

GProTray is licensed under the MIT License (see `LICENSE`).
The following MIT-licensed open-source projects were referenced or adapted for HID++ 2.0 protocol handling, onboard profile layouts, and device behavior:

1. **omm-rust** (https://github.com/rareinator/omm-rust)
   - License: MIT License (Copyright (c) 2026 Morten Nissen)
   - Used as reference / adapted for: HID++ `0x8100` onboard profile sector format decoding, directory parsing, and CRC-16/CCITT-FALSE verification.

2. **Moreno.LogiPro** (https://github.com/MorenoLand/Moreno.LogiPro)
   - License: MIT License
   - Used as reference for: Windows HID++ transport details on Logitech G Pro Wireless (`046D:C539`), `0x2201` AdjustableDPI, `0x8100` onboard profile handling, and preserving `0x8070` lighting state across onboard sector writes.

3. **libratbag** (https://github.com/libratbag/libratbag)
   - License: MIT License (Copyright (c) 2015-2017 Red Hat, Inc., David Herrmann)
   - Used as reference for: Logitech G Pro Wireless device matching (`046d:4079`, `046d:c088`) and HID++ 2.0 specification constants.
