# Routing Design

Milestone 0.0.3 inserts one immutable routing layer between Hyper and existing
handlers:

```text
Request -> HostRouter -> LocationRouter -> Handler -> Response
```

## HostRouter

`HostRouter` stores a small `Vec<VirtualHost>`. At the HTTP boundary it first
requires exactly one `Host` header and validates a restricted DNS hostname plus
optional `u16` port. For the usual origin-form request-target, that header is
the effective request authority. For absolute-form, the request-target
authority is validated, replaces `Host`, and becomes the effective authority
for both virtual-host selection and the request passed to the handler. This
implements the HTTP/1.1 requirement to ignore the received Host identity for
absolute-form without allowing routing and application-visible authority to
disagree.

Missing, duplicate, or malformed Host fields still return `400`, including for
absolute-form, because HTTP/1.1 requires a valid single field even when its
identity is superseded. A missing or invalid absolute authority also returns
`400`; an effective hostname that is not configured returns `404`. The port is
preserved for downstream handlers but ignored for virtual-host selection, and
hostname comparison is case-insensitive. Explicit port-based virtual hosts,
wildcards, bracketed IPv6 literal authorities, authority-form and asterisk-form
routing are intentionally unsupported.

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
