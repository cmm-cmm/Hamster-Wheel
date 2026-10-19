# Static File Opened-Handle Containment

Status: implemented in the working tree for owner review. It is not committed,
tagged or released. Windows only; native Linux is unchanged and unverified.

## Problem

The previous static path decided containment on a pathname and then opened the
file by that pathname:

```text
normalize -> pathname metadata -> canonicalize -> starts_with(root)
          -> File::open(canonical pathname) -> metadata(handle) -> serve handle
```

A controlled probe showed that the namespace can change between the
containment check and `File::open`. Final-file replacement, intermediate
directory replacement and junction replacement were all reproduced. In the
junction case the validated location was inside the document root while the
opened handle resolved outside it.

## Windows model

Windows static containment validates the final resolved location of the opened
file handle and serves that same handle:

```text
normalize -> open candidate (index selection only if that open is refused)
          -> GetFinalPathNameByHandleW(handle) -> starts_with(root)
          -> metadata(handle) -> GET of <= 64 KiB: read through the handle
                               -> larger GET streams handle / HEAD drops handle
```

No pathname is reopened after validation. The open, final-path query,
containment check, metadata and -- for a GET of at most 64 KiB -- the read all
run in one `spawn_blocking` call, so a small file costs one blocking-pool
round trip. Above that size the validated `std::fs::File` becomes the
`tokio::fs::File` owned by `ResponseBody::file`, so streaming, the 128 KiB
frame limit and Hyper backpressure are unchanged. GET and HEAD share one
resolution and therefore one containment decision. A file that shrank between
the metadata and the read is served as what was read, with that length.

### Representation

`std::fs::canonicalize` on Windows is itself `CreateFileW` plus
`GetFinalPathNameByHandleW(VOLUME_NAME_DOS)` (Rust 1.98.1 standard library).
The document root is canonicalized that way when configuration compiles, and
the opened handle is queried with the same flags, so both sides use the same
`\\?\C:\...` form and the names the filesystem stores. Measured on NTFS:

- canonical pathname and handle final path were byte-identical for a file, a
  nested file, an inside junction and an outside junction;
- opening a file through an upper-cased path returned the stored casing;
- `Path::starts_with` compares components, so `\\?\C:\www` does not contain
  `\\?\C:\www-other`; the drive letter compares case-insensitively; other
  components compare exactly; and a `\\?\` path never matches a non-verbatim
  one, which fails closed.

No lowercase or other string normalization is applied. Because both sides are
filesystem-reported names, exact comparison is also correct inside a
per-directory case-sensitive directory. Renaming the document root to
different casing after startup makes every request fail closed with 403.

### Reparse points

Junctions and symbolic links are followed by the open, as before. A target that
resolves inside the root is served; a target that resolves outside it is
rejected with 403. `FILE_FLAG_OPEN_REPARSE_POINT` is not used because it would
open the link rather than the object being served. Symbolic-link creation needs
a privilege this development machine does not hold, so only junctions have been
exercised here.

### Directory and index selection

The candidate is opened first. A plain open of a directory returns
`ERROR_ACCESS_DENIED`, and only on that failure does a pathname metadata lookup
decide whether to append `index.html`; access denied on anything else is
returned as it was. This is index selection, not containment: opening a
directory would need `FILE_FLAG_BACKUP_SEMANTICS`, and that flag also changes
access checks when the backup privilege is enabled. The served handle is
therefore always a plain read open. On Windows a metadata query is itself an
open, so a file now costs one open where it used to cost two (2026-10-19). If
the namespace changes after the lookup, whatever is opened is still validated
by its handle.

### Observed filesystem behavior

- Renaming a file after it is opened moves the handle's final path with it; the
  handle keeps reading the original object.
- NTFS refuses to rename a directory while a file below it is open.
- A file deleted after it is opened reports a final path under
  `\\?\C:\$Extend\$Deleted\`, so it is rejected with 403.

## Errors

| Case | Response |
|---|---|
| candidate or index not found | 404 |
| open denied | 403 |
| opened final location outside the root | 403 |
| final-path query failed | 500, operator log without a path |
| opened object is not a regular file | 404 |
| other I/O error | 500, operator log |

Response bodies are fixed strings and never contain a resolved path.

## Evidence

Unit tests in `src/static_files.rs` (`windows_containment_tests`) use a
test-only seam that pauses resolution after the open and after validation.
They cover inside and outside files, inside and outside junctions, a
`www-other` sibling reached through a junction, file replacement, junction
retargeting and intermediate-directory replacement after open, and a namespace
change after validation. Each seam test asserts that its namespace change ran.
The seam does not exist outside unit-test builds.

The tests were run against four deliberately wrong implementations. Reopening
the pathname after validation, taking HEAD metadata from the pathname, the old
canonicalize-then-open model, and a string-prefix containment check each fail
at least one of them.

Performance: see `benchmarks/results/static_containment_20260921.md`.

## Non-Windows

Non-Windows targets keep the pathname model: canonicalize, check, then open by
pathname. They do not have the opened-handle guarantee and are not qualified
against concurrent namespace changes. The code compiles for
`x86_64-unknown-linux-gnu`; that is a compile check, not native verification.

## Not established

UNC/SMB roots, volumes mounted in folders, ReFS, hard links and per-directory
case-sensitive directories have not been exercised. Content changed through the
same file object after validation is served as changed; this design fixes which
object is served, not its contents.
