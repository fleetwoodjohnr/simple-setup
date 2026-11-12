# Simple Setup 0.4.1

Completes the distribution-aware migration work introduced in 0.4.0.

- APT, pacman and apk installations can be collected with dependencies and restored without
  networking on the same distribution, release and processor. Collection does not modify the
  source's installed package database. Signatures remain subject to the destination's trust policy.
- Official upstream GitHub RPM assets can be selected for RPM destinations, checked against a
  published SHA-256, inspected for architecture and installed through DNF or Zypper.
  Vendor RPMs are separate from distribution-specific dependency sets. Their dependencies still
  require resolution on the destination; a vendor RPM alone is not promised offline-ready.
- The pre-build Why dialog includes the bundled choice and applicable alternatives,
  rejection reasons, disabled preferences and approval requirements.
- Compressed APT indexes receive package provenance checks. Approval requirements include
  fallback methods; execution rejects review-required methods absent from the displayed plan.
- Snapshot format 3 adds typed native package sets and vendor RPMs. Formats 1 and 2 remain
  readable without modification. Older restore engines require updating to read format 3.
- Reused and resumed installers are checked against the destination distribution.

## Verification

See STATUS.md for executed results. Reproduce disconnected package transactions with
scripts/test-native-sets.sh (Podman, a built static engine and internet for collection).
Everything installs inside disposable containers; source and destination use the same
locally resolved image. Restoration is repeated to verify idempotence.

Image-based systems remain excluded from package-file restores. Rolling distributions have
no fixed VERSION_ID: their solver must still accept the complete local transaction; equality
of distribution IDs is not a guarantee against package conflicts. No forced dependency bypass
or automatic distribution upgrade is performed.

The existing published v0.4.0 release is retained. This completion ships as v0.4.1.
