/*

State
    - new_flashblock_received(...)
        - update
    - new_canonical_block_received(...)
        - clear
    - subscribe_to_flashblocks(...)
        - return a thing that fires on new one received and processed

Cache
    - current block number
    - pending block
    - map<hash => receipt>
    - map<hash => txn>
    - map<address => balance>
    - map<address => txn count>

    new_flashblock_received(...)
        - appends data to maps

 */
// use alloy_primitives::{Address, U256};
// use std::collections::HashMap;
//
// struct FlashblocksCache {
//     account_balances: HashMap<Address, U256>,
// }
//
// impl FlashblocksCache {
//     // pub fn new(flashblock: FlashblocksPayloadV1) {
//     //     todo!()
//     // }
//     //
//     // pub fn on_flashblock_received(&self, flashblock: FlashblocksPayloadV1) {
//     //     todo!()
//     // }
//     //
//     // fn insert_data(&self, flashblock: FlashblocksPayloadV1) {
//     //     todo!()
//     // }
// }
