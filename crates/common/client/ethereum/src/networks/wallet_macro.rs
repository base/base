/// Utility to implement IntoWallet for signer over the specified network.
#[macro_export]
macro_rules! impl_into_wallet {
    ($(@[$($generics:tt)*])? $signer:ty) => {
        impl $(<$($generics)*>)? $crate::IntoWallet for $signer {
            type NetworkWallet = $crate::EthereumWallet;
            fn into_wallet(self) -> Self::NetworkWallet {
                $crate::EthereumWallet::from(self)
            }
        }

    };
}
