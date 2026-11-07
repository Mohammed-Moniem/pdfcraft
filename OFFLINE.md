# Offline components and integration

All download destinations below are under **Mohammed-Moniem/embedded-office-suite**, your account. No Cargo registry access is needed to run the prebuilt native/WASM baseline. Existing approved Node/WebView is the integration host; the Rust compiler/linker/OS SDK/system tar are rebuild prerequisites.

- [Exact component downloads, hashes and versions](https://github.com/Mohammed-Moniem/embedded-office-suite/releases/download/v0.1.0-progress.4/COMPONENT-DOWNLOADS.md)
- [Complete suite source4 with SDKs, build scripts and provenance](https://github.com/Mohammed-Moniem/embedded-office-suite/releases/download/v0.1.0-progress.4/office-source-progress4-r1.tar.gz)
- [Offline build instructions](https://github.com/Mohammed-Moniem/embedded-office-suite/blob/main/docs/BUILDING.md)
- [Private Node/WebView host integration contract](https://github.com/Mohammed-Moniem/embedded-office-suite/blob/main/docs/HOST-INTEGRATION.md)
- [Official documented feature comparison and limits](https://github.com/Mohammed-Moniem/embedded-office-suite/blob/main/docs/FEATURE-COMPARISON.md)

Use the complete suite source4 layout for the existing verified offline build recipes; this repository presents the individual engine at its root. Preserve Cargo.lock and the Git revision pins. Vendor/tool/support archives are listed in the component guide, with Rust/Cargo1.95.0 and matching WASM tools. The native and WASM packages include all four components and their packaged SDKs; select pdfcraft through that SDK. They are shared assets on your account, not duplicated or newly qualified binaries.

The original .github files are preserved byte-for-byte under UPSTREAM-GITHUB/ as inactive source references. No workflow is installed under .github/workflows/, and repository Actions settings are unchanged. Review and deliberately adapt those references before activating CI. This publication does not start builds or install dependencies.
