//! Macros for shared contract client boilerplate.

macro_rules! contract_call {
    ($call:expr, $context:expr) => {
        $call.await.map_err(|error| $crate::ContractError::call($context, error))
    };
}
