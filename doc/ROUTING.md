# Routing Design

Milestone 0.0.3 inserts one immutable routing layer between Hyper and existing
handlers:

```text
Request -> HostRouter -> LocationRouter -> Handler -> Response
```

## HostRouter

`HostRouter` stores a small `Vec<VirtualHost>`. It reads exactly one `Host`
header, validates a restricted DNS hostname plus optional `u16` port, ignores
the port for selection, and compares hostnames case-insensitively. Missing,
duplicate, or malformed Host values return `400`; an unconfigured hostname
returns `404`. Wildcards are intentionally unsupported.

A vector avoids allocating a lowercase hostname on every request. It is the
simplest representation for the two hard-coded milestone hosts; a different
index requires benchmark evidence and a larger real configuration.

## LocationRouter

Locations are normalized and sorted by descending prefix length once at
startup. Request lookup is a linear longest-prefix scan with path-segment
boundaries: `/api` matches `/api` and `/api/users`, but not `/apix`. `/` is the
root catch-all. Invalid configuration and duplicates fail during construction.

## Handler

`Handler` is a small enum rather than an async trait or framework abstraction.
`Static` delegates directly to the milestone 0.0.2 static implementation and
holds its pre-canonicalized document root. `Simple` constructs fixed responses
and currently supplies the `/api` placeholder and routing errors.

The router is built once, shared by `Arc`, and never mutated. Request routing
does not parse configuration, canonicalize the document root, rebuild tables,
or acquire a global mutex.
