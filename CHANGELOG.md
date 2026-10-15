# Changelog

## 1.3.0 - 2026-10-15

- Add explicit ordinary-Repentance cloud selection without timestamp guessing.
  Preliminary Repentance+ format adaptation remains disabled pending further testing.
- Add stable-save monitoring and automatic local-only uploads from Isaac's menu.
- Replace the circular cloud control with a compact top-right menu-only Steam button and
  synchronized/pending/attention border colors.
- Keep all three slots selectable for Force Push/Pull even before a successful
  comparison; report a busy operation instead of silently ignoring the tap.
- Exclude the verified save-load counter from the three-way comparison, without
  changing uploaded bytes or weakening full-file hash/CRC verification. Restore
  old comparison BASEs only from exact matching snapshots or verified backups.
- Add ordinary-Repentance progression/Continue bundle hashing, same-slot UFS
  batches, read-back verification, and recoverable local replacement.
- Verify Continue's CRC and progression checksum link; refuse mismatched pairs.
- Back up and restore matching progression/run pairs without inventing a BASE.
- Confirm ordinary Repentance Continue loading through Steam Cloud in both
  directions with PC Lua mods disabled.
- Add opt-in PC mod-state adaptation with a bounded structural parser and
  checksum relinking. This does not port custom mod content.
- Confirm a separate ordinary Repentance round trip with PC Lua mods enabled:
  iOS upload, Windows Continue and Save and Exit, then native iOS Continue.
- Capture save identities during construction and hold at native app-delegate
  initialization, before game-view initialization. Do not wait under dyld's
  image-initialization lock; keep one bounded deadline and refuse late replacement.
- Repentance+ runs and rerun snapshots are not supported.

## 1.2.0 - 2026-08-20

- Renamed the project and release artifacts to IsaacSteamSynciOS to reflect
  both Steam Cloud and achievement synchronization.

- Added one-way Steam achievement synchronization from native Isaac
  `persistentgamedata` saves when the user taps **Sync Now**.
- Added only save-derived unlocks missing from Steam; existing Steam
  achievements and stat bits are preserved and never cleared.
- Replaced the obsolete UserStats write with the current
  `CMsgClientStoreUserStats2`/EMsg 5466 request, current CRC, and both SteamID
  fields.
- Serialized achievement and Cloud operations through the same authenticated
  Steam session to prevent competing CM logins.
- Added post-write read-back verification, bounded request timeouts, strict
  save-section validation, and additive-safety regression tests.
- Removed all runtime dependence on Game Center and added a build audit that
  rejects GameKit/Game Center linkage.

## 1.1.4 - 2026-08-13

- Added a read-only native menu/game detector for the supported Isaac build.
- Made the Steam Cloud button menu-only and automatically dismissed an open
  settings panel when gameplay begins.
- Resolve Isaac's native player-list vector so the detector cannot remain
  stuck on an inactive main-menu player allocation.

## 1.1.3 - 2026-08-13

Initial public release as IsaacSteamCloudSynciOS.

- Added direct Steam QR/device and username/password authentication with Steam
  Guard approval and Keychain refresh-token persistence.
- Added verified AppID 250900 Cloud enumeration, download, upload, commit, and
  re-enumeration.
- Added canonical iOS raw-LZ4 to Windows save conversion and validation.
- Added last-known-common three-way synchronization, first-sync protection,
  explicit conflict resolution, and manual publication with Sync Now.
- Added local and remote backups, retention, restoration, atomic replacement,
  and interruption-safe recovery.
- Added remote-only prelaunch pulls, offline fallback, lifecycle handling, and
  Steam rich presence.
- Added one portable ARM64 dylib for rootless ElleKit injection and embedded
  non-jailbroken installation.
- Added UIKit account, sync, backup, log, conflict, and invisible-button UI.
- Added IPA patching, signing support, dependency auditing, and comprehensive
  Rust tests.
