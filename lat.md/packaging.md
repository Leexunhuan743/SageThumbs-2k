# Packaging

How the workspace builds and ships: artifact naming, the release pipeline, and the installer layout that makes the shell extension load cleanly on a fresh Windows 11 machine.

## Release Pipeline

`scripts/build-release.ps1` drives the full pipeline, always setting `-C target-feature=+crt-static` and producing the Inno Setup installer plus the MSIX.

A plain `cargo build --release` without `crt-static` links the DLL against the MSVC CRT dynamically and can fail `regsvr32` with `0x8007007E`. The pipeline builds the EXEs with the `html-preview`/`webp-lossy` features and a slim DLL with `dll-i18n-subset`, bundles the hardened ImageMagick (`packaging/imagemagick-source.json` + `packaging/imagemagick-policy.xml`), and produces `packaging/installer.iss` plus the MSIX via `packaging/make-msix.ps1`. `packaging/size-budget.json` enforces the download budget; `scripts/verify.ps1` gates the release and `scripts/release.ps1` orchestrates it.

## DLL and EXE Conventions

The three artifacts install side-by-side and are resolved relative to the DLL at runtime, never via the current EXE (which inside the shell host is `explorer.exe`/`dllhost.exe`).

[[src/lib.rs#dll_main]] captures the HMODULE, a `sibling_of_dll` helper resolves siblings (returning `None` for a DLL-only install), and the artifact names are the `APP_EXE`/`CLI_EXE` constants in `src/lib.rs`. The cdylib's PDB is redirected to `sagethumbs2k_dll.pdb` in `dll/build.rs` to avoid the case-folding PDB collision with the `SageThumbs2K.exe` bin under `cargo test`.
