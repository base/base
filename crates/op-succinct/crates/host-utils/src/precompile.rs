use revm::precompile::{Bytes, Precompile, Precompiles};
use revm::primitives::Address;

/// This precompile hook substitutes the precompile with a custom one that can stub out the logic
/// for specific operations that we don't have precompiles for. Used in `create_hook_precompile`.
pub fn precompile_hook(_env: sp1_sdk::HookEnv, buf: &[u8]) -> Vec<Vec<u8>> {
    let addr: Address = buf[0..20].try_into().unwrap();
    let gas_limit = u64::from_le_bytes(buf[20..28].try_into().unwrap());
    let input: Bytes = buf[28..].to_vec().into();
    println!("[HOOK] Precompile addr {} called.", addr);

    // Note: Fetch the latest precompiles because the gas costs are different from older versions.
    // Otherwise, the hooked precompiles will fail.
    let precompiles = Precompiles::latest();

    let precompile = precompiles.inner().get(&addr).unwrap();
    let result = match precompile {
        Precompile::Standard(precompile) => precompile(&input, gas_limit),
        _ => panic!("Annotated precompile must be a standard precompile."),
    };

    let mut serialized_vec = vec![];
    match result {
        Ok(result) => {
            serialized_vec.push(0);
            serialized_vec.extend_from_slice(&result.gas_used.to_le_bytes());
            serialized_vec.extend_from_slice(&result.bytes);
        }
        Err(err) => {
            serialized_vec.push(1);
            match err {
                revm::precompile::PrecompileErrors::Error(_) => {
                    serialized_vec.push(0);
                }
                revm::precompile::PrecompileErrors::Fatal { .. } => {
                    serialized_vec.push(1);
                }
            }
        }
    }
    vec![serialized_vec]
}
