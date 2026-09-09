//! Prints contract-backed upgrade IDs in registration order.

use base_common_chain_activation::ContractUpgradeIds;

fn main() {
    println!("{}", ContractUpgradeIds::csv());
}
