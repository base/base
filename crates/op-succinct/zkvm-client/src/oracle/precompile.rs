use alloy_primitives::aliases::{U8, U32, U128, U256, U384, U512, U1024};
use alloc::vec::Vec;
use anyhow::{anyhow, Result};

extern crate alloc;

#[repr(u8)]
pub enum Precompile {
    ECRECOVER(U256, U256, U256, U256) = 1,
    SHA256(Vec<u8>) = 2,
    RIPEMD160(Vec<u8>) = 3,
    ID(Vec<u8>) = 4,
    MODEXP(U256, U256, U256, Vec<u8>, Vec<u8>, Vec<u8>) = 5,
    ECADD(U256, U256, U256, U256) = 6,
    ECMUL(U256, U256, U256) = 7,
    ECPAIRING(U256, U256, U256, U256, U256, U256) = 8,
    BLAKE2F(U32, U512, U1024, U128, U8) = 9,
    POINTEVAL(U256, U256, U256, U384, U384) = 10
}

impl Precompile {
    pub fn from_bytes(hint_data: &Vec<u8>) -> Result<Self> {
        let (addr, input) = hint_data.split_at(20);
        let addr = u128::from_be_bytes(addr.try_into().unwrap());

        let precompile = match addr {
            1 => {
                if input.len() != 128 {
                    return Err(anyhow!("wrong input length"));
                }
                let hash = U256::from_be_bytes::<32>(input[0..32].try_into().unwrap());
                let v = U256::from_be_bytes::<32>(input[32..64].try_into().unwrap());
                let r = U256::from_be_bytes::<32>(input[64..96].try_into().unwrap());
                let s = U256::from_be_bytes::<32>(input[96..128].try_into().unwrap());

                Ok(Self::ECRECOVER(hash, v, r, s))
            },
            2 => Ok(Self::SHA256(input.to_vec())),
            3 => Ok(Self::RIPEMD160(input.to_vec())),
            4 => Ok(Self::ID(input.to_vec())),
            5 => {
                let b_size = U256::from_be_bytes::<32>(input[0..32].try_into().unwrap());
                let e_size = U256::from_be_bytes::<32>(input[32..64].try_into().unwrap());
                let m_size = U256::from_be_bytes::<32>(input[64..96].try_into().unwrap());

                let b_size_as_usize = usize::try_from(b_size).unwrap();
                let e_size_as_usize = usize::try_from(e_size).unwrap();
                let m_size_as_usize = usize::try_from(m_size).unwrap();

                if input.len() != 96 + b_size_as_usize + e_size_as_usize + m_size_as_usize {
                    return Err(anyhow!("wrong input length"));
                };

                let b = input[96..96 + b_size_as_usize].to_vec();
                let e = input[96 + b_size_as_usize..96 + b_size_as_usize + e_size_as_usize].to_vec();
                let m = input[96 + b_size_as_usize + e_size_as_usize..].to_vec();

                Ok(Self::MODEXP(b_size, e_size, m_size, b, e, m))
            }
            6 => {
                if input.len() != 128 {
                    return Err(anyhow!("wrong input length"));
                }

                let x1 = U256::from_be_bytes::<32>(input[0..32].try_into().unwrap());
                let y1 = U256::from_be_bytes::<32>(input[32..64].try_into().unwrap());
                let x2 = U256::from_be_bytes::<32>(input[64..96].try_into().unwrap());
                let y2 = U256::from_be_bytes::<32>(input[96..128].try_into().unwrap());

                Ok(Self::ECADD(x1, y1, x2, y2))
            }
            7 => {
                if input.len() != 96 {
                    return Err(anyhow!("wrong input length"));
                }

                let x = U256::from_be_bytes::<32>(input[0..32].try_into().unwrap());
                let y = U256::from_be_bytes::<32>(input[32..64].try_into().unwrap());
                let k = U256::from_be_bytes::<32>(input[64..96].try_into().unwrap());

                Ok(Self::ECMUL(x, y, k))
            }
            8 => {
                if input.len() != 192 {
                    return Err(anyhow!("wrong input length"));
                }

                let x1 = U256::from_be_bytes::<32>(input[0..32].try_into().unwrap());
                let y1 = U256::from_be_bytes::<32>(input[32..64].try_into().unwrap());
                let x2 = U256::from_be_bytes::<32>(input[64..96].try_into().unwrap());
                let y2 = U256::from_be_bytes::<32>(input[96..128].try_into().unwrap());
                let x3 = U256::from_be_bytes::<32>(input[128..160].try_into().unwrap());
                let y3 = U256::from_be_bytes::<32>(input[160..192].try_into().unwrap());

                Ok(Self::ECPAIRING(x1, y1, x2, y2, x3, y3))
            }
            9 => {
                if input.len() != 212 {
                    return Err(anyhow!("wrong input length"));
                }

                let rounds = U32::from_be_bytes::<4>(input[0..4].try_into().unwrap());
                let h = U512::from_be_bytes::<64>(input[4..68].try_into().unwrap());
                let m = U1024::from_be_bytes::<128>(input[68..180].try_into().unwrap());
                let t = U128::from_be_bytes::<16>(input[180..196].try_into().unwrap());
                let f = U8::from_be_bytes::<1>(input[196..197].try_into().unwrap());

                Ok(Self::BLAKE2F(rounds, h, m, t, f))
            }
            10 => {
                if input.len() != 192 {
                    return Err(anyhow!("wrong input length"));
                }

                let hash = U256::from_be_bytes::<32>(input[0..32].try_into().unwrap());
                let x = U256::from_be_bytes::<32>(input[32..64].try_into().unwrap());
                let y = U256::from_be_bytes::<32>(input[64..96].try_into().unwrap());
                let commitment = U384::from_be_bytes::<48>(input[96..144].try_into().unwrap());
                let proof = U384::from_be_bytes::<48>(input[144..192].try_into().unwrap());

                Ok(Self::POINTEVAL(hash, x, y, commitment, proof))
            }
            _ => return Err(anyhow!("unknown precompile")),
        };

        precompile
    }

    pub fn execute(&self) -> Vec<u8> {
        match self {
            Precompile::ECRECOVER(hash, v, r, s) => unimplemented!(),
            Precompile::SHA256(data) => unimplemented!(),
            Precompile::RIPEMD160(data) => unimplemented!(),
            Precompile::ID(data) => unimplemented!(),
            Precompile::MODEXP(b_size, e_size, m_size, b, e, m) => unimplemented!(),
            Precompile::ECADD(x1, y1, x2, y2) => unimplemented!(),
            Precompile::ECMUL(x, y, k) => unimplemented!(),
            Precompile::ECPAIRING(x1, y1, x2, y2, x3, y3) => unimplemented!(),
            Precompile::BLAKE2F(rounds, h, m, t, f) => unimplemented!(),
            Precompile::POINTEVAL(hash, x, y, commitment, proof) => unimplemented!(),
        }
    }
}
