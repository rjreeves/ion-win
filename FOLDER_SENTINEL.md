# Folder Sentinel

Folder Sentinel is a small, complete ion-win app that answers a high-stakes
question without an agent, cloud account, database, or background service:

> Has anything in this folder changed since I trusted it?

It creates a SHA-256 baseline for every file in a folder and later compares a
fresh scan against that baseline. Added, removed, and modified files are shown
separately. Every verification also emits a checksummed CSV snapshot, making
the result easy to archive, inspect in Excel, or feed into another ion-win
pipeline.

## Why this is an ion-win app

The implementation is a readable Ion script, but its core is not text parsing.
`find` produces paths, `stat` enriches them concurrently with metadata and
SHA-256 hashes, and the values remain structured tables through comparison and
CSV serialization. File names containing spaces are data, not shell syntax.

The safety boundary is simple: verification never changes the protected
folder, and sealing refuses to overwrite an existing baseline.

## Run it

Build ion-win, then seal a folder. Keep the sentinel directory outside the
folder being protected.

```powershell
cargo build --release
target\release\ion-win.exe examples\folder_sentinel.ion seal C:\Critical C:\Sentinels\critical-v1
```

Type `seal` at the confirmation prompt. Verify it later:

```powershell
target\release\ion-win.exe examples\folder_sentinel.ion verify C:\Critical C:\Sentinels\critical-v1
```

Exit status `0` means the folder matches. Exit status `1` means drift was
detected, so the command works naturally in CI, Task Scheduler, or an ion-win
task. Usage and missing-input errors return status `2`.

## Produced evidence

- `baseline.csv` — the trusted path, size, modification time, and SHA-256 rows;
- `baseline_sha256.csv` — checksum of the trusted manifest itself;
- `latest_scan.csv` — the newest verification snapshot;
- `latest_scan_sha256.csv` — checksum of that snapshot.

The baseline is never replaced. To accept an intentional change, seal into a
new sentinel directory; that creates a visible history of trust decisions
instead of silently moving the goalposts.

## The killer workflow

Use Folder Sentinel for release artifacts, legal evidence, configuration
folders, portable drives, photo archives, or a known-clean incident-response
toolkit. Once useful interactively, promote the same command into ion-win's
task and schedule layer for unattended checks with process-tree cancellation
and resource policies.
