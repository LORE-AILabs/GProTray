# Third-Party Notices and References

GProTray is licensed under the MIT License (see [LICENSE](LICENSE)).

## Referenced / Adapted MIT-Licensed Projects

The following MIT-licensed open-source projects were referenced or adapted for HID++ 2.0 protocol handling, onboard profile layouts, and device behavior:

1. **omm-rust** (https://github.com/rareinator/omm-rust)
   - Copyright (c) 2026 Morten Nissen
   - License: MIT License
   - Referenced / adapted for: HID++ `0x8100` onboard profile sector format decoding/encoding, directory parsing, HID keyboard usage mappings, and CRC-16/CCITT-FALSE verification.

2. **Moreno.LogiPro** (https://github.com/MorenoLand/Moreno.LogiPro)
   - Copyright (c) 2026 MorenoLand
   - License: MIT License
   - Referenced for: Windows HID++ transport details on Logitech G Pro Wireless (`046D:C539`), `0x2201` AdjustableDPI, `0x8100` onboard profile handling, and preserving `0x8070` lighting state across onboard sector writes.

3. **libratbag** (https://github.com/libratbag/libratbag)
   - Copyright (c) 2015-2017 Red Hat, Inc.
   - Copyright (c) 2015 David Herrmann <dh.herrmann@gmail.com>
   - License: MIT License
   - Referenced for: Logitech G Pro Wireless device matching (`046d:4079`, `046d:c088`) and HID++ 2.0 feature/LED layout specifications.

### MIT License (applicable to the projects listed above)

```text
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice (including the next
paragraph where applicable) shall be included in all copies or substantial
portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Runtime Crate Dependencies

- **windows-sys** (`0.59.0`) and **windows-targets** (`0.52.6`)
  - License: `MIT OR Apache-2.0`
