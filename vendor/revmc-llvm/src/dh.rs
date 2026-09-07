use std::{ffi::c_void, fmt, ptr};

use inkwell::{
    context::{AsContextRef, Context},
    llvm_sys::{LLVMDiagnosticHandler, LLVMDiagnosticSeverity::*, core::*, prelude::*},
};

/// LLVM diagnostic handler guard.
pub(crate) struct DiagnosticHandlerGuard {
    cx: LLVMContextRef,
    prev_dh: LLVMDiagnosticHandler,
    prev_dhc: *mut c_void,
}

impl fmt::Debug for DiagnosticHandlerGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiagnosticHandlerGuard").finish_non_exhaustive()
    }
}

impl DiagnosticHandlerGuard {
    pub(crate) fn new(cx: &Context) -> Self {
        unsafe {
            let c = cx.as_ctx_ref();
            let prev_dh = LLVMContextGetDiagnosticHandler(c);
            let prev_dhc = LLVMContextGetDiagnosticContext(c);
            LLVMContextSetDiagnosticHandler(c, Some(Self::diagnostic_handler), ptr::null_mut());
            Self { cx: c, prev_dh, prev_dhc }
        }
    }

    extern "C" fn diagnostic_handler(di: LLVMDiagnosticInfoRef, _context: *mut c_void) {
        unsafe {
            // `LLVMGetDiagInfoDescription` returns an LLVM `Message`.
            let msg_cstr = crate::llvm_string(LLVMGetDiagInfoDescription(di));
            let msg = msg_cstr.to_string_lossy();
            match LLVMGetDiagInfoSeverity(di) {
                LLVMDSError => error!(target: "llvm", "{msg}"),
                LLVMDSWarning => warn!(target: "llvm", "{msg}"),
                LLVMDSRemark => trace!(target: "llvm", "{msg}"),
                LLVMDSNote => debug!(target: "llvm", "{msg}"),
            }
        }
    }
}

impl Drop for DiagnosticHandlerGuard {
    fn drop(&mut self) {
        unsafe {
            LLVMContextSetDiagnosticHandler(self.cx, self.prev_dh, self.prev_dhc);
        }
    }
}
