# Attribution

The project is a Rust reimplementation of the Windows equivalents of
[m1ng-li/mac-thermalright-ai-monitor](https://github.com/m1ng-li/mac-thermalright-ai-monitor),
itself based on [beret21/MacTR](https://github.com/beret21/MacTR).
The reference README identifies its code as MIT licensed. LY protocol constants,
device profiles, frame packet layout, dashboard arrangement, and local agent log
interpretation were derived from that reference. Original authors retain their
copyrights.

The LY protocol was reverse engineered by
[Lexonight1/thermalright-trcc-linux](https://github.com/Lexonight1/thermalright-trcc-linux).

The two embedded PNGs were decoded from the reference project's Swift assets:

- `assets/BongoCat.png`: Bongo Cat artwork from
  [kuroni/bongocat-osu](https://github.com/kuroni/bongocat-osu). Artwork rights belong
  to its original creators; this project's MIT license does not relicense it.
- `assets/Pikachu.png`: official Pikachu artwork distributed in
  [PokeAPI/sprites](https://github.com/PokeAPI/sprites).
  Pokémon © Nintendo / Creatures / GAME FREAK. This project's MIT license does
  not grant rights to this artwork. It is included as decoration matching the
  reference. Replace it before distributing if your use requires other rights.

Windows fonts are loaded from the operating system and are not bundled.
Rust dependencies retain their individual licenses; `Cargo.lock` records exact
versions. libusb is LGPL-2.1-or-later; the vendored build links it into the binary.
When distributing binaries, comply with its LGPL relinking/source requirements.
The full application source and build scripts permit rebuilding against a modified
libusb; a binary package alone does not include all dependency source code.
