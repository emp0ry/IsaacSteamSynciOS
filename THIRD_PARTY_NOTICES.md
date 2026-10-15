# Third-party notices

IsaacSteamSynciOS statically links Rust crates listed in `src/core/Cargo.lock`.
Their license declarations and source distributions remain authoritative.

Notable direct dependencies:

- `steam-cm-protocol` 0.4.1 — MIT license —
  <https://crates.io/crates/steam-cm-protocol>. A source copy is retained under
  `vendor/steam-cm-protocol`; its vulnerable unconditional RSA dependency is
  removed. QR/device and credential authentication use the same Valve CM
  protocol. See the vendoring note in that directory.
- `tokio` — MIT license — <https://github.com/tokio-rs/tokio>
- `reqwest` — MIT/Apache-2.0 — <https://github.com/seanmonstar/reqwest>
- `rustls` (through reqwest and Steam transport) — Apache-2.0/ISC/MIT —
  <https://github.com/rustls/rustls>
- `prost` — Apache-2.0 — <https://github.com/tokio-rs/prost>
- RustCrypto `sha1` and `sha2` — MIT/Apache-2.0 —
  <https://github.com/RustCrypto/hashes>
- `zip` — MIT license — <https://github.com/zip-rs/zip2>

The minimal Steam Cloud protobuf field definitions correspond to the public
Steam protocol definitions maintained by SteamDatabase:
<https://github.com/SteamDatabase/Protobufs>.

The Repentance save CRC interoperability behavior was cross-checked against
the public reverse-engineering utility `IsaacSaveFileCrc`:
<https://github.com/bladecoding/IsaacSaveFileCrc>.

Steam, Steam Cloud, Steam Guard, and Steam Mobile are trademarks of Valve
Corporation. This project is independent and is not endorsed by Valve,
Nicalis, or Apple.

## Save-format research

Thanks to [n1cknam3l3ss/IsaacSteamSynciOS](https://github.com/n1cknam3l3ss/IsaacSteamSynciOS)
for identifying the Repentance+ progression layout differences. This project's
adapter validates section boundaries and checksums, preserves the additional
fields verbatim, and does not adopt automatic achievement/counter merging.

The save-load trailer counter was independently checked against private native
fixtures and the format notes in [REPENTOGON's SaveFile.h](https://github.com/TeamREPENTOGON/REPENTOGON/blob/main/repentogon/SaveSyncing/SaveFile.h).
This is a format reference only; no REPENTOGON implementation code is bundled.
