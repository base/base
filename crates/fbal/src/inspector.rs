use alloy_eip7928::{AccountChanges, BalanceChange, CodeChange, NonceChange};
use alloy_primitives::{Address, FixedBytes, U256, map::foldhash::HashMap};
use revm::{
    Inspector,
    bytecode::opcode,
    context::{ContextTr, JournalTr, Transaction},
    inspector::JournalExt,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter,
        interpreter_types::{InputsTr, Jumps},
    },
    primitives::StorageKey,
};

#[derive(Debug, Default)]
pub struct TouchedAccountsInspector {
    pub touched_accounts: HashMap<Address, Vec<FixedBytes<32>>>,
    // pub account_changes: HashMap<Address, AccountChanges>,
}

impl<CTX> Inspector<CTX> for TouchedAccountsInspector
where
    CTX: ContextTr<Journal: JournalExt>,
{
    fn step(&mut self, interp: &mut Interpreter, _context: &mut CTX) {
        match interp.bytecode.opcode() {
            opcode::SLOAD => {
                let slot = interp.stack.peek(0).expect("should be able to load slot");
                let contract = interp.input.target_address();
                self.touched_accounts.entry(contract).or_default().push(slot.into());
            }
            opcode::SSTORE => {
                // TODO: This is likely not needed as it can be gotten from the block executor
                //
                // let slot = interp.stack.peek(0).expect("should be able to load slot");
                // let new_value = interp.stack.peek(1).expect("should be able to load new value");
                // let contract = interp.input.target_address();
            }
            opcode::EXTCODECOPY | opcode::EXTCODEHASH | opcode::EXTCODESIZE | opcode::BALANCE => {
                let slot = interp.stack.peek(0).expect("should be able to load slot");
                let addr = Address::from_word(slot.into());
                println!("EXTCODECOPY | EXT CODE HASH | EXT CODE SIZE | BALANCE {}", addr);
                self.touched_accounts.entry(addr).or_default();
            }
            opcode::DELEGATECALL | opcode::CALL | opcode::STATICCALL | opcode::CALLCODE => {
                let addr_slot = interp.stack.peek(1).expect("should be able to load slot");
                let addr = Address::from_word(addr_slot.into());
                println!("DELEGATECALL | CALL | STATICCALL | CALLCODE {}", addr);
                self.touched_accounts.entry(addr).or_default();
            }

            _ => {}
        }
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.touched_accounts.entry(inputs.target_address).or_default();
        self.touched_accounts.entry(inputs.caller).or_default();

        // let caller_account = context.journal_mut().load_account(inputs.caller).ok()?;
        // let is_eoa_caller = caller_account.info.is_empty_code_hash();
        // if is_eoa_caller {
        //     caller_entry.nonce_changes.push(NonceChange::new(0, caller_account.info.nonce));
        // }

        None
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        // let nonce = context.journal_mut().load_account(inputs.caller).ok()?.data.info.nonce;
        // self.account_changes.entry(inputs.caller).or_default();

        // let deployed_contract = inputs.created_address(nonce);
        // self.account_changes
        //     .entry(deployed_contract)
        //     .or_default()
        //     .code_changes
        //     .push(CodeChange::new(0, inputs.init_code.clone()));

        None
    }
}
