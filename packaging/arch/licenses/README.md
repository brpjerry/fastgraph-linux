These crates omit their license files from the published crate, or embed a
font without its notice. The collector copies the notices listed for exactly
that name and version in `fallbacks.json`, beside any the crate publishes
itself; any other crate without a notice fails the package build rather than
inventing a license grant. The project's GPL-3.0-or-later license
does not relicense these third-party notices.

Each file is copied verbatim from the upstream commit recorded in the crate's
`.cargo_vcs_info.json`, except where noted:

| Crates | Files | Upstream |
| --- | --- | --- |
| cookie-factory 0.3.3 | `cookie-factory/MIT.txt` | rust-bakery/cookie-factory `d36b805dbd7dd65f2df947235c5bcc573afe2c76` (`LICENSES/MIT.txt`) |
| gpu-alloc 0.6.2, gpu-alloc-types 0.3.1 | `gpu-alloc/` | zakarumych/gpu-alloc `61020bdcb4eb04fca6bbf748aeb9570e859236e0` (identical at `7ddd1fe576fcfc503347eb97e476a47581479f22`) |
| gpu-descriptor 0.3.2, gpu-descriptor-types 0.2.0 | `gpu-descriptor/` | zakarumych/gpu-descriptor `bb28b4e55b742e0ea3e7a0968e7ee6287ba0e06a` (identical at `9f2fb9546591322e7b77ee1e79385e01effd85e4`) |
| iced_core, iced_debug, iced_futures, iced_graphics, iced_program, iced_renderer, iced_runtime, iced_selector, iced_wgpu 0.14.0; iced_widget 0.14.2; iced_winit 0.14.1; iced_tiny_skia 0.14.1 | `iced/LICENSE` | iced-rs/iced tag 0.14.0 `3997291f318a8bc06fa522f5579836fb3feb94df` (identical at `54abf81d…` and `38237dd2…`). iced_tiny_skia 0.14.1 records `0ecf6066…`, which is not on GitHub; `LICENSE` has not changed since 2019 (`10b07b80…`), so it is the same file |
| iced_graphics 0.14.0 | `iced/FiraSans-OFL.txt` | The `fira-sans` feature embeds `fonts/FiraSans-Regular.ttf` (Fira Sans Regular Version 4.203, SHA-256 `3dabf3d4…5e97cd97`; name table: "Digitized data copyright 2012-2016, The Mozilla Foundation and Telefonica S.A.", "Licensed under the Open Font License, version 1.1 or later"). iced's repository has no notice for it. The file is the OFL of Google Fonts' Fira Sans 4.203, the same version, verbatim: google/fonts `6ebe737e44fd64ee736bbc6feed6136a6dbfd222` (`ofl/firasans/OFL.txt`). The two font files differ in bytes, so it is the notice for that version, not for an identical file. |
| iced_graphics 0.14.0 | `iced/Iced-Icons-OFL.txt` | Always embedded `fonts/Iced-Icons.ttf` (SHA-256 `2289860d…bfebb7ec`; name table: "Copyright (C) 2020 by original authors @ fontello.com"). Its check, caret and chevron glyphs match Font Awesome 4.7.0's (`fontawesome-webfont.ttf`, outlines compared point by point); iced's repository has no notice for them. Written here: a note on the glyphs, Font Awesome's attribution, the copyright line from that font's name table, and the OFL 1.1 text from `iced/FiraSans-OFL.txt` (from its `-----` line on) |
| sctk-adwaita 0.10.1 | `sctk-adwaita/Cantarell-OFL.txt` | winit's `wayland-csd-adwaita` feature embeds `src/title/Cantarell-Regular.ttf` (Cantarell Regular Version 0.05, SHA-256 `d15f25bc…6e93b55f`) to draw client-side title bars; the crate's `LICENSE` covers only its code. Written here: the font's own copyright and license statement (name table entry 0, verbatim but for CRLF line ends), then the OFL 1.1 text from `iced/FiraSans-OFL.txt` (from its `-----` line on) |
| lyon 1.0.19, lyon_algorithms 1.0.22, lyon_geom 1.0.20, lyon_path 1.0.19, lyon_tessellation 1.0.23 | `lyon/` | nical/lyon `56219c95c658d4f39de7b190d98e57d5773ba241` (identical at `db98d26a1a90076a7a894262dcb51117eef643c8`) |
| profiling 1.0.18 | `profiling/` | aclysma/profiling `8271551172eb6fa4cba47369aedd93790c623df9` |
| spirv 0.3.0+sdk-1.3.268.0 | `rspirv/LICENSE` | gfx-rs/rspirv `e91a5debc10c395b36db7721dfff6326d697a1e2` |
| svg_fmt 0.4.5 | `rust_debug/LICENSE` | nical/rust_debug `93f414cd572b5c2781df93dbdc068ed13443d05a` (MIT, one of its `MIT/Apache-2.0` options) |
| hexf-parse 0.2.1 | `hexf/CC0-1.0.txt` | lifthrasiir/hexf `4225763d744183d720f575ae96d04161b4d08ea0` ships no license text; its Cargo.toml declares `CC0-1.0`. The file is the SPDX CC0-1.0 text from Arch's `/usr/share/licenses/spdx/` |

When `Cargo.lock` moves one of these crates to another version, the build
fails until its notice is checked and its entry updated.
