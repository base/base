//! ABI definitions for the `foo` reference precompile.

use alloy_sol_types::sol;

sol! {
    /// `foo` reference precompile ABI.
    ///
    /// The interface is append-only across versions: methods and errors are
    /// added, never removed or repurposed, so historical calldata keeps
    /// decoding the same way.
    interface IFoo {
        /// Precompile cannot be executed via delegatecall or callcode.
        error DelegateCallNotAllowed();

        /// The called method is not supported by the version active at this block.
        ///
        /// Returned for methods that had not yet been introduced at the target
        /// hardfork, preserving their original pre-activation revert behavior.
        error UnsupportedBeforeActivation();

        /// Emitted when `greet` records a greeting for `caller`.
        event Greeted(address indexed caller, string greeting);

        /// Returns a fixed greeting. The value is version-specific and frozen
        /// once its version is activated.
        function helloWorld() external view returns (string);

        /// Records and returns a personalized greeting for the caller.
        ///
        /// Introduced in a later version; reverts with
        /// `UnsupportedBeforeActivation` before its activation fork.
        function greet(string name) external returns (string);
    }
}
