# Inspect and resolve sync conflicts

Ordinary directory synchronization compares both trees with their last successful common baseline. A conflict stops the sync plan. Inspect the conflicting paths with the same profile and roots used for synchronization:

```sh
rdrive conflict list work "$HOME/Documents/Work" shared
```

For a regular file that changed on both sides, preview the repair using `--choose local` to keep the local contents, or `--choose remote` to keep the remote contents:

```sh
rdrive --json conflict repair work "$HOME/Documents/Work" shared notes.txt --choose local
```

Review the file details and copy the returned `token`:

```sh
rdrive conflict repair work "$HOME/Documents/Work" shared notes.txt \
  --choose local --apply --expect TOKEN
```

RDrive checks that both versions still match the preview. It saves the displaced content under `.rdrive-versions/` inside the local sync root before replacing it. The result includes its relative `recovery_path`. A successful repair advances the baseline for that file only. Run `conflict list` again to check other paths before resuming synchronization.

The repair requires an existing common file baseline and regular files on both sides. Keeping local content requires a backend that supports conditional replacement. Handle deletion conflicts, directory conflicts, and file/directory collisions by inspecting and recovering each side before running sync again.

For conflicts in a Finder Smart domain, see [Smart operations and recovery](SMART_OPERATIONS.md). Smart domains retain queued mutations and apply their own conflict decisions.
