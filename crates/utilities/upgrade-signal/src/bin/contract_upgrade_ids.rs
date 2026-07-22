//! Prints contract-backed upgrade IDs in registration order.

use base_upgrade_signal::ContractUpgradeIds;

fn main() {
    println!("{}", ContractUpgradeIds::csv());
}
