//! Ordered comparison of packed-semver protocol versions.

use core::cmp::Ordering;

use alloy_primitives::U256;

/// A `ProtocolVersions` packed-semver value ordered by its semver fields.
///
/// The L1 `ProtocolVersions` contract stores each version as a `uint256` whose highest byte is the
/// version-type. For the only defined layout, version-type `0`, the value is
/// `version-type || reserved || build || major || minor || patch || pre-release`, where `major`,
/// `minor`, `patch`, and `pre-release` are the low four 32-bit fields, `build` occupies the next 64
/// bits, and `reserved`/`version-type` occupy the high 64 bits.
///
/// Ordering follows the protocol-version semver rules rather than the raw integer value:
///
/// * an unrecognized version-type (`version-type != 0`) sorts *above* every version-type-`0` value,
///   so a compatibility check against a version-type-`0` node stays fail-closed for a format the
///   node cannot interpret;
/// * within version-type `0`, compare `major`, then `minor`, then `patch`;
/// * a pre-release (`pre-release != 0`) sorts *below* its matching release (`pre-release == 0`),
///   and pre-releases of the same `major.minor.patch` sort by their pre-release counter;
/// * `build` and the reserved high bits are ignored, per spec.
///
/// A raw `U256` comparison instead sorts a pre-release *above* its release, because the pre-release
/// field holds a larger integer than the release's zero. That inverts the intended order and can
/// reject a node running a final release under a minimum that is a release candidate of the same
/// version.
#[derive(Debug, Clone, Copy, Eq)]
pub struct PackedProtocolVersion(U256);

impl PartialEq for PackedProtocolVersion {
    /// Equality mirrors [`Ord`]: two values are equal when their ordered fields (version-type plus
    /// the semver fields) match, so `build` and the reserved bits are ignored. Deriving `PartialEq`
    /// instead would compare the raw `U256` bit-for-bit and break the `Ord` contract, under which
    /// `a.cmp(&b) == Ordering::Equal` must imply `a == b`.
    fn eq(&self, other: &Self) -> bool {
        self.ordering_key() == other.ordering_key()
    }
}

impl PackedProtocolVersion {
    /// Wraps a packed protocol version read from the L1 `ProtocolVersions` contract.
    pub const fn new(packed: U256) -> Self {
        Self(packed)
    }

    /// Packs the ordered semver fields into the version-type `0` layout, leaving `build` and the
    /// reserved/version-type bytes zero.
    pub const fn pack(major: u32, minor: u32, patch: u32, prerelease: u32) -> Self {
        Self(U256::from_limbs([
            ((patch as u64) << 32) | prerelease as u64,
            ((major as u64) << 32) | minor as u64,
            0,
            0,
        ]))
    }

    /// Returns the underlying packed `U256` value.
    pub const fn into_inner(self) -> U256 {
        self.0
    }

    /// Major version field.
    pub const fn major(self) -> u32 {
        (self.0.as_limbs()[1] >> 32) as u32
    }

    /// Minor version field.
    pub const fn minor(self) -> u32 {
        self.0.as_limbs()[1] as u32
    }

    /// Patch version field.
    pub const fn patch(self) -> u32 {
        (self.0.as_limbs()[0] >> 32) as u32
    }

    /// Pre-release counter field; `0` denotes a final release.
    pub const fn prerelease(self) -> u32 {
        self.0.as_limbs()[0] as u32
    }

    /// Version-type byte (the highest byte of the `uint256`); `0` is the only defined layout.
    pub const fn version_type(self) -> u8 {
        (self.0.as_limbs()[3] >> 56) as u8
    }

    /// Ordering key that applies the version-type and semver pre-release rules.
    ///
    /// The version-type is the most significant component, so an unrecognized version-type
    /// (`version_type != 0`) sorts above every version-type-`0` value; a compatibility check against
    /// a version-type-`0` node therefore rejects a format the node cannot interpret (fail-closed).
    ///
    /// Within a version-type, a final release (`prerelease == 0`) is promoted above every
    /// pre-release of the same `major.minor.patch` by ranking it as [`u64::MAX`], which no 32-bit
    /// pre-release counter can reach.
    const fn ordering_key(self) -> (u8, u32, u32, u32, u64) {
        let prerelease_rank =
            if self.prerelease() == 0 { u64::MAX } else { self.prerelease() as u64 };
        (self.version_type(), self.major(), self.minor(), self.patch(), prerelease_rank)
    }
}

impl Ord for PackedProtocolVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.ordering_key().cmp(&other.ordering_key())
    }
}

impl PartialOrd for PackedProtocolVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_packed_semver_fields() {
        let version = PackedProtocolVersion::pack(1, 2, 3, 4);

        assert_eq!(version.major(), 1);
        assert_eq!(version.minor(), 2);
        assert_eq!(version.patch(), 3);
        assert_eq!(version.prerelease(), 4);
    }

    #[test]
    fn release_sorts_above_its_prerelease() {
        let release = PackedProtocolVersion::pack(1, 2, 3, 0);
        let prerelease = PackedProtocolVersion::pack(1, 2, 3, 1);

        // The raw integers order the prerelease higher; the semver ordering must not.
        assert!(prerelease.into_inner() > release.into_inner());
        assert!(prerelease < release);
    }

    #[test]
    fn prereleases_sort_by_counter() {
        let rc1 = PackedProtocolVersion::pack(1, 2, 3, 1);
        let rc2 = PackedProtocolVersion::pack(1, 2, 3, 2);

        assert!(rc1 < rc2);
    }

    #[test]
    fn orders_by_major_then_minor_then_patch() {
        assert!(PackedProtocolVersion::pack(1, 2, 3, 0) < PackedProtocolVersion::pack(1, 2, 4, 0));
        assert!(PackedProtocolVersion::pack(1, 2, 3, 0) < PackedProtocolVersion::pack(1, 3, 0, 0));
        assert!(PackedProtocolVersion::pack(1, 2, 3, 0) < PackedProtocolVersion::pack(2, 0, 0, 0));

        // A higher patch release outranks a lower-patch pre-release regardless of pre-release rank.
        assert!(PackedProtocolVersion::pack(1, 2, 3, 9) < PackedProtocolVersion::pack(1, 2, 4, 0));
    }

    #[test]
    fn build_field_does_not_affect_ordering() {
        // Set the build field (bits 128..192) on one operand; ordering must ignore it.
        let plain = PackedProtocolVersion::pack(1, 2, 3, 0);
        let with_build = PackedProtocolVersion::new(plain.into_inner() | (U256::from(7) << 128));

        // The raw integers differ, but the ordered fields (and thus both `cmp` and `==`) agree,
        // keeping the `Ord`/`PartialEq` contract intact.
        assert_ne!(plain.into_inner(), with_build.into_inner());
        assert_eq!(plain.cmp(&with_build), Ordering::Equal);
        assert_eq!(plain, with_build);
    }

    #[test]
    fn unknown_version_type_sorts_above_every_version_type_zero_value() {
        // The version-type occupies the highest byte (bits 248..255).
        let unknown = PackedProtocolVersion::new(U256::from(1) << 248);
        let highest_type_zero = PackedProtocolVersion::pack(u32::MAX, u32::MAX, u32::MAX, 0);

        assert_eq!(unknown.version_type(), 1);
        assert_eq!(highest_type_zero.version_type(), 0);
        // Even the largest possible version-type-0 value ranks below any unknown version-type, so a
        // version-type-0 node treats an unrecognized format as unsupported (fail-closed).
        assert!(highest_type_zero < unknown);
    }
}
