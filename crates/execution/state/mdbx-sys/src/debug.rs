use crate::{MDBX_chk_scope, MDBX_chk_scope__bindgen_ty_1};

impl core::fmt::Debug for MDBX_chk_scope__bindgen_ty_1 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The active union field is determined by MDBX, so do not read either field here.
        f.debug_struct("MDBX_chk_scope__bindgen_ty_1").finish_non_exhaustive()
    }
}

impl core::fmt::Debug for MDBX_chk_scope {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MDBX_chk_scope")
            .field("stage", &self.stage)
            .field("verbosity", &self.verbosity)
            .field("subtotal_issues", &self.subtotal_issues)
            .finish_non_exhaustive()
    }
}
