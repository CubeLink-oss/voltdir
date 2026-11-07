# Privacy

Effective 2 November 2026. Applies to voltdir 0.0.2 and to the Claude Code plugin that bundles it.

voltdir is a read-only disk analysis tool. It runs on your computer and sends nothing anywhere. This page says what it reads, what it writes, and what it does not do.

## What voltdir reads

- The names, sizes, timestamps and attributes of the files and folders you point it at. With `info`, also the volumes on your computer and their free space.
- It does not read, parse or keep file contents. The `mft` engine reads a volume's file table, which is where Windows stores file metadata; the contents of very small files can physically sit inside that table, and voltdir extracts only names, sizes and timestamps from it.
- File and folder names can contain personal information, for example a person's name in a folder called "Alice's taxes". voltdir does not look for personal information. It reports names as they are. By default the paths in its output are relative to the folder you scanned.

## What voltdir writes

- Only what you ask for: the file given to `--out`, the index given to `--save-index`, the snapshot given to `--save-snapshot`, and the two small files written next to a snapshot, `.idx` and `.nidx` (a name index; turn both off with `--no-index`). `.nidx` holds only what the snapshot already holds: names, sizes, times and where each line sits in the snapshot.
- While writing, it creates a temporary file in the destination folder and renames it when the write is complete. If the write fails, the temporary file is removed.
- These files contain file and folder paths and sizes. They stay on your computer until you delete them. Treat them like a directory listing of your disk, because that is what they are.
- voltdir never deletes, moves or changes any file or folder that it scans.

## What voltdir does not do

- It does not open network connections. The program uses none of the networking components of Windows, and it contains no analytics, telemetry or update check. If you point it at a network share (a `\\server\share` path), Windows reads that share for it, as it would for any program.
- It does not collect, store or share data with its developer or with anyone else.
- It does not raise its own privileges. The `mft` engine needs administrator rights, which you grant by running voltdir as administrator.

## When an AI agent runs voltdir

voltdir prints its results as JSON on standard output. If an AI agent such as Claude runs voltdir for you, the agent reads that output, so the file and folder names and sizes it contains become part of your conversation with the agent. How that conversation is handled is governed by the terms and privacy policy of the AI service you use, not by voltdir.

voltdir does not decide which folders are off limits: it scans whatever you point it at, including folders whose names are personal, because that may be exactly what you want. The skill that ships with voltdir instead asks the agent to confirm with you before it scans a whole drive, a personal folder, or a folder whose name suggests private content, unless you named that folder yourself. That is guidance to the agent, not a technical block. If some folders should stay private, do not ask the agent to scan them, or keep the results in files and share only the summary.

## Retention

voltdir keeps nothing. The files it writes are yours to keep or delete.

## Children

voltdir is a developer and IT tool. It is not directed at children.

## Changes

If a future version changes anything on this page, this page changes in the same release, and the change is visible in the repository history.

## Contact

Questions and reports: [GitHub Issues](https://github.com/CubeLink-oss/voltdir/issues).
