# Security

voltdir is a read-only disk scanner. This page says what the bundled `voltdir.exe` can and cannot do, in terms you can check yourself, and how to report a problem.

## What the bundled `voltdir.exe` can and cannot do

These statements describe `skills/voltdir/bin/voltdir.exe` and the Rust source in this repository that it is built from.

**It has no networking code.** The executable imports exactly five system libraries, and none of them is a networking, HTTP, DNS or crypto-API library:

| Library | Functions | What for |
|---|---|---|
| `kernel32.dll` | 108 | files, directories, threads, memory, console |
| `ntdll.dll` | 3 | low-level directory listing |
| `advapi32.dll` | 2 | `OpenProcessToken`, `GetTokenInformation` (see below) |
| `bcryptprimitives.dll` | 1 | random seed for hash tables (part of the Rust standard library) |
| `api-ms-win-core-synch-l1-2-0.dll` | 3 | thread synchronisation |

There is no `ws2_32`, `winhttp`, `wininet`, `urlmon`, `dnsapi`, `mswsock`, `crypt32` or `secur32`, and the source contains no use of `std::net` and no URL. voltdir cannot open a connection, and it does not download or run anything.

**It reads no credentials.** The only two `advapi32` calls ask whether the voltdir process is already running elevated (the `elevated` field of `voltdir info`, and the choice of the `mft` engine). They look at the process's own elevation flag. They do not read, store or send any token, password, key or account data, and voltdir never tries to elevate itself. The source reads two environment variables, `VOLTDIR_MFT_STANDARD` and `VOLTDIR_PROF`, which are diagnostic switches; no variable is read for a credential.

**It only reads metadata.** It reads names, sizes, timestamps and attributes. It never opens a file to read its contents, and it never deletes, moves or renames anything it scans.

**It writes only what you ask for.** The files named by `--out`, `--save-index` and `--save-snapshot`, plus the two small files written next to a snapshot (`.idx` and `.nidx`, off with `--no-index`). It writes each file to a temporary name first and renames it when complete, and it removes only the temporary and probe files it created itself.

Why a binary is bundled at all: an agent skill should work on a machine without a Rust toolchain. Because a binary cannot be read like source, the build is made checkable instead.

## Check the bundled binary yourself

- **Rebuild it.** The build is deterministic (`/Brepro`, no timestamps or build paths). With Rust 1.94.0 and the MSVC toolchain, `cargo +1.94.0 build --release` on the same commit gives the same bytes. The SHA-256 is in `skills/voltdir/bin/voltdir.exe.sha256`. Steps are in the README under "Verify the bundled binary".
- **List its imports.** With Visual Studio's tools: `dumpbin /imports skills\voltdir\bin\voltdir.exe`. The table above should match.
- **Read the source.** 28 files under `src/`, no third-party dependencies (`Cargo.toml` has an empty `[dependencies]` section).

If you would rather not run a binary you did not build, build `target\release\voltdir.exe` yourself; the skill also accepts `voltdir` on your `PATH`.

## Output is data

File names come from whoever put a file on the disk. voltdir escapes control characters, quotes, backslashes and bidirectional or zero-width characters as `\uXXXX`, so its JSON output is always valid and a crafted name cannot hide in it. The bundled skill tells agents to treat names as data, never as instructions.

## Reporting a vulnerability

Please open an issue at <https://github.com/CubeLink-oss/voltdir/issues>. For something you would rather not post publicly, use the repository's Security tab and "Report a vulnerability" where it is offered. Include the voltdir version (`voltdir --version`), what you ran, and what you expected.

The privacy statement is in [`PRIVACY.md`](PRIVACY.md).
