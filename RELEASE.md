# Release process

This document describes how to cut a new mergeFox release and the
secrets / certificates required to produce signed, notarized artifacts.

The workflow file lives at `.github/workflows/release.yml` — it fires
on tag pushes that match `v*` and also via manual dispatch.

---

## 1. Cut a release

```bash
# 1. Update the version in Cargo.toml + RELEASE_NOTES.md
# 2. Commit
git commit -am "release: v0.1.0-alpha.2"

# 3. Tag + push
git tag v0.1.0-alpha.2
git push origin main --tags
```

Tags that contain `-alpha`, `-beta`, or `-rc` are marked **pre-release**
on GitHub. Everything else is a full release.

GitHub Actions will:

1. Build for macOS (arm64 + x86_64), Windows x64, Linux x64.
2. Apply code signing **iff** the relevant secrets are set.
3. Notarize macOS binaries **iff** notarization secrets are set.
4. Upload artifacts to a **draft** GitHub Release — you must review +
   publish manually so accidental tag pushes do not ship.

---

## 2. Code signing — macOS

### Requirements
- **Apple Developer Program** membership ($99/year)
- **Developer ID Application** certificate (not "Mac App Distribution")
- An **app-specific password** for `notarytool`

### Export the cert

In Keychain Access on the machine that has the Developer ID:

```bash
# Export as .p12 with a password — save to a safe place.
# Keychain Access → right-click the cert → Export → .p12
base64 -i DeveloperID.p12 | pbcopy
```

### Secrets to add (GitHub → Settings → Secrets and variables → Actions)

| Name | Value |
|---|---|
| `APPLE_SIGNING_CERT` | Base64-encoded `.p12` (the `pbcopy` above) |
| `APPLE_SIGNING_CERT_PASSWORD` | The password you used for the `.p12` |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` |
| `APPLE_NOTARIZE_USER` | Your Apple ID email |
| `APPLE_NOTARIZE_PASSWORD` | App-specific password (appleid.apple.com → Sign-In and Security) |
| `APPLE_TEAM_ID` | 10-char team ID from developer.apple.com |

When none of these are set, the release workflow still produces an
**unsigned** binary (Gatekeeper will warn users, but the build succeeds).

---

## 3. Code signing — Windows

### Requirements
- OV (Organization Validation) or **EV** (Extended Validation) code
  signing certificate. EV hits SmartScreen less aggressively — worth the
  premium if budget allows.

### Secrets to add

| Name | Value |
|---|---|
| `WINDOWS_SIGNING_CERT` | Base64-encoded `.pfx` |
| `WINDOWS_SIGNING_CERT_PASSWORD` | `.pfx` password |

EV certs typically require a hardware token (YubiKey / eToken) and
cannot be exported as `.pfx` — for EV you will need a dedicated signing
runner. That is out of scope for the current workflow; open an issue if
you need EV support added.

---

## 4. Linux

Linux binaries are currently shipped as unsigned `.tar.gz`. Future work
(see [`TODO/production.md`](./TODO/production.md) §A2):

- Sign AppImage with GPG
- Publish `.sig` alongside each artifact
- Homebrew tap / Flathub / winget once the signing story is solid

---

## 5. Verifying a build locally

Before pushing a tag, test the release binary:

```bash
cargo build --release --locked
./target/release/mergefox
```

Check the version banner in the app matches `Cargo.toml`.

---

## 6. Post-release checklist

- [ ] GitHub Release is published (not draft) with the correct notes
- [ ] Artifacts are attached for macOS arm64, macOS x86_64, Windows, Linux
- [ ] macOS builds pass `spctl --assess --type execute --verbose mergefox`
- [ ] Windows builds are signed (right-click → Properties → Digital Signatures)
- [ ] `Cargo.toml` version matches the tag
- [ ] `RELEASE_NOTES.md` has an entry for this version
- [ ] Announce — README badge, discussions, social
