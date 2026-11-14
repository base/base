//! Object paths of the archive bucket layout (TDD §2.5, with epoch-scoped segment directories).

use object_store::path::Path;

/// Paths of every archive object under an optional prefix.
///
/// ```text
/// <prefix>/catalog.json
/// <prefix>/dict/<domain>-v<n>
/// <prefix>/seg/<k>/<owner_epoch>/<file>
/// <prefix>/idx/
/// ```
///
/// Segment objects live under the writer epoch that uploaded them (a deviation from the TDD's
/// `seg/<k>/<file>`). Object stores have no conditional DELETE, so a superseded garbage
/// collector can always issue one last delete after its ownership check; scoping paths by
/// epoch means that delete can only reach objects of an older owner, never the objects a
/// newer owner uploads and publishes. The index manifest `m‖k` records which epoch directory
/// holds the published files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveLayout {
    /// Prefix every object lives under; empty for the bucket root.
    pub prefix: String,
}

impl ArchiveLayout {
    /// Name of the catalog object.
    pub const CATALOG: &'static str = "catalog.json";
    /// Directory holding segment objects.
    pub const SEGMENTS: &'static str = "seg";
    /// Directory holding the `SlateDB` index.
    pub const INDEX: &'static str = "idx";

    /// Creates a layout rooted at `prefix` (leading and trailing slashes are ignored).
    pub fn new(prefix: impl AsRef<str>) -> Self {
        Self { prefix: prefix.as_ref().trim_matches('/').to_string() }
    }

    /// Joins `rest` onto the prefix.
    pub fn join(&self, rest: &str) -> Path {
        if self.prefix.is_empty() {
            Path::from(rest)
        } else {
            Path::from(format!("{}/{rest}", self.prefix))
        }
    }

    /// Path of `catalog.json`.
    pub fn catalog(&self) -> Path {
        self.join(Self::CATALOG)
    }

    /// Path of the `SlateDB` index root.
    pub fn index(&self) -> Path {
        self.join(Self::INDEX)
    }

    /// Path of the directory holding every segment.
    pub fn segments(&self) -> Path {
        self.join(Self::SEGMENTS)
    }

    /// Path of the directory holding every epoch of segment `k`.
    pub fn segment_dir(&self, k: u64) -> Path {
        self.join(&format!("{}/{k}", Self::SEGMENTS))
    }

    /// Path of the directory holding the objects of segment `k` uploaded under `epoch`.
    pub fn segment_epoch_dir(&self, k: u64, epoch: u64) -> Path {
        self.join(&format!("{}/{k}/{epoch}", Self::SEGMENTS))
    }

    /// Path of object `file` of segment `k` uploaded under `epoch`.
    pub fn segment_object(&self, k: u64, epoch: u64, file: &str) -> Path {
        self.join(&format!("{}/{k}/{epoch}/{file}", Self::SEGMENTS))
    }

    /// Parses the segment number out of a `seg/<k>/...` path under this prefix.
    pub fn parse_segment(&self, path: &Path) -> Option<u64> {
        let segments = self.segments();
        let mut parts = path.prefix_match(&segments)?;
        parts.next()?.as_ref().parse().ok()
    }

    /// Parses `(k, epoch)` out of a `seg/<k>/<epoch>/<file>` path under this prefix; `None` for
    /// paths that do not follow the epoch-scoped layout.
    pub fn parse_object(&self, path: &Path) -> Option<(u64, u64)> {
        let segments = self.segments();
        let mut parts = path.prefix_match(&segments)?;
        let k = parts.next()?.as_ref().parse().ok()?;
        let epoch = parts.next()?.as_ref().parse().ok()?;
        parts.next()?;
        Some((k, epoch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths honour the prefix and parse back to the segment number and epoch.
    #[test]
    fn paths_round_trip() {
        let layout = ArchiveLayout::new("/845337/");
        assert_eq!(layout.catalog().as_ref(), "845337/catalog.json", "catalog path");
        assert_eq!(layout.index().as_ref(), "845337/idx", "index path");
        let object = layout.segment_object(12, 3, "headers.dat");
        assert_eq!(object.as_ref(), "845337/seg/12/3/headers.dat", "segment object path");
        assert_eq!(layout.parse_segment(&object), Some(12), "segment number parses back");
        assert_eq!(layout.parse_object(&object), Some((12, 3)), "segment and epoch parse back");
        assert_eq!(
            layout.parse_object(&Path::from("845337/seg/12/headers.dat")),
            None,
            "a path without an epoch directory is not an archive object"
        );
        assert_eq!(
            ArchiveLayout::new("").catalog().as_ref(),
            "catalog.json",
            "empty prefix is the bucket root"
        );
    }
}
