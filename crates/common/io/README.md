# `base-common-io`

File operations with errors that retain the affected path. `Files` provides reads, JSON files,
directory operations, and atomic writes; `FsPathError` preserves the underlying I/O error.

Atomic writes use a temporary file in the destination directory, sync the file, rename it, and sync
the directory. A failed write callback preserves the existing destination and removes the temporary file.
