# Security policy

## Supported versions

mergeFox is in **alpha** (`v0.1.0-alpha.x`). Only the **latest alpha
release on `main`** is supported for security fixes. Once we cut a
beta, that and the latest alpha will both be supported for 30 days to
allow migration; after that, beta only.

| Version | Status | Security fixes |
|---|---|---|
| `main` | active alpha | ✅ |
| `v0.1.0-alpha.x` (any) | active alpha | ✅ (latest only) |
| anything older | unreleased | N/A |

## Reporting a vulnerability

**Do not open a public GitHub issue for security bugs.** A Git client
holds credentials (OAuth tokens, PATs, SSH keys on disk) and can
execute the system `git` binary on user-provided URLs — both are
sensitive attack surfaces, and we want a chance to ship a fix before
the vulnerability becomes common knowledge.

Instead, use GitHub's private vulnerability reporting:

> https://github.com/JeoungMyeoungHo/MergeFox/security/advisories/new

If that channel is unavailable for any reason, email the maintainer
listed in `Cargo.toml`'s `authors` field with a subject line starting
`[mergeFox security]`.

Please include:

1. A clear description of the vulnerability — what it is, where it
   lives in the code (file + line if you have it), and what an attacker
   can do with it.
2. A minimal reproduction. A screen recording or a scripted repro both
   work; a fuzz-harness hit is gold.
3. Your disclosure preference: we default to **90 days** of private
   coordination before public disclosure, but will accept shorter
   (for active exploitation) or longer (for complex fixes) on request.

You can expect:

- Acknowledgement within **72 hours**.
- A triage decision (is it a security issue, and at what severity)
  within **7 days**.
- For confirmed issues: a private security advisory, a CVE request
  where applicable, and credit in the fix commit + release notes
  unless you prefer to remain anonymous.

## Scope

**In scope** for this policy:

- The `mergefox` binary itself — memory safety, logic bugs, auth
  handling, credential storage, command injection into the system
  `git` binary, URL parsing that reaches `git ls-remote` / `fetch` /
  `clone`, AI endpoint request construction.
- OS-keychain / `secrets.json` file handling (permissions, exposure
  via logs, exposure via crash dumps).
- MCP transport once it ships (tracked as `TODO/features.md` §5).
- CI / release workflows under `.github/workflows/` that could be
  abused to leak signing material.

**Out of scope** for this policy (these are bugs, not security
issues — file a normal issue):

- The system `git` binary's own behavior. If `git` itself has a CVE,
  report to them; we'll pick up the fix in our documentation and
  `git` version requirement.
- Social-engineering attacks that depend on the user manually pasting
  a hostile URL + clicking Clone. We document the risk and do basic
  URL filtering (`file://`, `ext::`); we don't promise to catch every
  variant.
- Denial-of-service from the user cloning a 500 GB repo on a laptop.
  The app will struggle; that's expected.
- Third-party dependencies' vulnerabilities — those show up in
  `cargo audit` (run weekly in CI) and we upgrade on a normal cadence.

## Known-sensitive surfaces

If you are doing a security review and want a starting map of where
to look:

| Surface | Where to look | What to stress |
|---|---|---|
| Credential storage | `src/secrets.rs` | File permissions on non-macOS (Windows ACL — known gap, `TODO/production.md` §B4) |
| HTTPS credential injection | `src/git/jobs.rs::build_cmd_with_creds` | The `!f() { … }` inline helper — any way to escape into the shell? |
| URL parsing before `git` call | `src/git_url.rs`, `src/clone.rs` | `file://`, `ext::`, `scp`-shorthand with pathological hosts |
| AI endpoint request | `src/ai/client.rs` | Can user-controlled fields affect the URL/path? SSRF through `base_url`? |
| OAuth token scope at rest | `src/secrets.rs` + `src/providers/oauth.rs` | What's stored, for how long, in what form |

## Disclosure philosophy

We prefer **coordinated disclosure** but will not use legal threats,
takedown notices, or the CFAA against a good-faith researcher. If you
report something and we disagree about severity or timeline, we will
try to work it out in the advisory thread before anyone goes public.

Thank you for looking.
