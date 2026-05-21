//! Benchmarks for Base-native token and token-factory precompile logic.

use std::hint::black_box;

use alloy_primitives::{Address, B256, U256};
use alloy_sol_types::SolValue;
use base_common_precompiles::{
    B20Token, B20TokenStorage, Burnable, Configurable, IB20, ITokenFactory, Mintable, Pausable,
    PolicyHandle, Token, TokenAccounting, TokenFactoryStorage, TokenVariant, Transferable,
};
use base_precompile_storage::{HashMapStorageProvider, StorageCtx};
use criterion::{Criterion, criterion_group, criterion_main};

struct BaseTokenBenchSetup;

impl BaseTokenBenchSetup {
    const fn admin() -> Address {
        Address::repeat_byte(0xad)
    }

    const fn caller() -> Address {
        Address::repeat_byte(0xca)
    }

    const fn initial_supply_recipient() -> Address {
        Address::repeat_byte(0xcd)
    }

    fn token_params(name: &str, symbol: &str) -> ITokenFactory::B20CreateParams {
        ITokenFactory::B20CreateParams {
            version: TokenFactoryStorage::CREATE_TOKEN_VERSION,
            name: name.to_string(),
            symbol: symbol.to_string(),
            initialAdmin: Self::admin(),
        }
    }

    fn create_b20(
        ctx: StorageCtx<'_>,
        caller: Address,
        params: ITokenFactory::B20CreateParams,
        salt: B256,
        _initial_supply: U256,
    ) -> Address {
        let call = ITokenFactory::createTokenCall {
            variant: ITokenFactory::TokenVariant::DEFAULT,
            salt,
            params: params.abi_encode().into(),
            initCalls: Vec::new(),
        };
        let mut factory = TokenFactoryStorage::new(ctx);
        factory.create_token(caller, call).unwrap()
    }

    fn create_token<'a>(
        ctx: StorageCtx<'a>,
        salt: B256,
        initial_supply: U256,
    ) -> B20Token<B20TokenStorage<'a>, PolicyHandle<'a>> {
        let params = Self::token_params("BaseToken", "BASE");

        let token_address = Self::create_b20(ctx, Self::caller(), params, salt, initial_supply);
        let mut token = Self::token_at(ctx, token_address);
        if initial_supply > U256::ZERO {
            token
                .mint(Self::admin(), Self::initial_supply_recipient(), initial_supply, true)
                .unwrap();
        }
        token
    }

    fn token_at<'a>(
        ctx: StorageCtx<'a>,
        token_address: Address,
    ) -> B20Token<B20TokenStorage<'a>, PolicyHandle<'a>> {
        B20Token::with_storage_and_policy(
            B20TokenStorage::from_address(token_address, ctx),
            PolicyHandle::new(ctx),
        )
    }
}

fn base_token_metadata(c: &mut Criterion) {
    c.bench_function("base_token_name", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let token = BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x01), U256::ZERO);

            b.iter(|| {
                let token = black_box(&token);
                let result = token.accounting().name().unwrap();
                black_box(result);
            });
        });
    });

    c.bench_function("base_token_symbol", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let token = BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x02), U256::ZERO);

            b.iter(|| {
                let token = black_box(&token);
                let result = token.accounting().symbol().unwrap();
                black_box(result);
            });
        });
    });

    c.bench_function("base_token_decimals", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let token = BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x03), U256::ZERO);

            b.iter(|| {
                let token = black_box(&token);
                let result = token.accounting().decimals().unwrap();
                black_box(result);
            });
        });
    });

    c.bench_function("base_token_contract_uri", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let token = BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x04), U256::ZERO);

            b.iter(|| {
                let token = black_box(&token);
                let result = token.accounting().contract_uri().unwrap();
                black_box(result);
            });
        });
    });
}

fn base_token_view(c: &mut Criterion) {
    c.bench_function("base_token_total_supply", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let token = BaseTokenBenchSetup::create_token(
                ctx,
                B256::repeat_byte(0x05),
                U256::from(1_000u64),
            );

            b.iter(|| {
                let token = black_box(&token);
                let result = token.accounting().total_supply().unwrap();
                black_box(result);
            });
        });
    });

    c.bench_function("base_token_balance_of", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let account = BaseTokenBenchSetup::initial_supply_recipient();
            let token = BaseTokenBenchSetup::create_token(
                ctx,
                B256::repeat_byte(0x06),
                U256::from(1_000u64),
            );

            b.iter(|| {
                let token = black_box(&token);
                let account = black_box(account);
                let result = token.accounting().balance_of(account).unwrap();
                black_box(result);
            });
        });
    });

    c.bench_function("base_token_allowance", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let owner = Address::repeat_byte(0x01);
            let spender = Address::repeat_byte(0x02);
            let mut token =
                BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x07), U256::ZERO);
            token.approve(owner, spender, U256::from(500u64)).unwrap();

            b.iter(|| {
                let token = black_box(&token);
                let owner = black_box(owner);
                let spender = black_box(spender);
                let result = token.accounting().allowance(owner, spender).unwrap();
                black_box(result);
            });
        });
    });

    c.bench_function("base_token_supply_cap", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let token = BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x08), U256::ZERO);

            b.iter(|| {
                let token = black_box(&token);
                let result = token.accounting().supply_cap().unwrap();
                black_box(result);
            });
        });
    });

    c.bench_function("base_token_paused", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let token = BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x09), U256::ZERO);

            b.iter(|| {
                let token = black_box(&token);
                let result = token.accounting().paused().unwrap();
                black_box(result);
            });
        });
    });

    c.bench_function("base_token_minimum_redeemable", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let token = BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x0b), U256::ZERO);

            b.iter(|| {
                let token = black_box(&token);
                let result = token.accounting().minimum_redeemable().unwrap();
                black_box(result);
            });
        });
    });
}

fn base_token_mutate(c: &mut Criterion) {
    c.bench_function("base_token_mint", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let user = Address::repeat_byte(0x01);
            let mut token =
                BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x0c), U256::ZERO);

            b.iter(|| {
                let token = black_box(&mut token);
                let user = black_box(user);
                token.mint(user, user, U256::ONE, true).unwrap();
            });
        });
    });

    c.bench_function("base_token_burn", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let holder = BaseTokenBenchSetup::initial_supply_recipient();
            let mut token = BaseTokenBenchSetup::create_token(
                ctx,
                B256::repeat_byte(0x0d),
                U256::from(u128::MAX),
            );

            b.iter(|| {
                let token = black_box(&mut token);
                let holder = black_box(holder);
                token.burn(holder, holder, U256::ONE, true).unwrap();
            });
        });
    });

    c.bench_function("base_token_approve", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let owner = Address::repeat_byte(0x01);
            let spender = Address::repeat_byte(0x02);
            let mut token =
                BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x0e), U256::ZERO);

            b.iter(|| {
                let token = black_box(&mut token);
                let owner = black_box(owner);
                let spender = black_box(spender);
                token.approve(owner, spender, U256::from(500u64)).unwrap();
            });
        });
    });

    c.bench_function("base_token_transfer", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let from = BaseTokenBenchSetup::initial_supply_recipient();
            let to = Address::repeat_byte(0x02);
            let mut token = BaseTokenBenchSetup::create_token(
                ctx,
                B256::repeat_byte(0x0f),
                U256::from(u128::MAX),
            );

            b.iter(|| {
                let token = black_box(&mut token);
                let from = black_box(from);
                token.transfer(from, to, U256::ONE).unwrap();
            });
        });
    });

    c.bench_function("base_token_transfer_from", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let owner = BaseTokenBenchSetup::initial_supply_recipient();
            let spender = Address::repeat_byte(0x02);
            let recipient = Address::repeat_byte(0x03);
            let mut token = BaseTokenBenchSetup::create_token(
                ctx,
                B256::repeat_byte(0x10),
                U256::from(u128::MAX),
            );
            token.approve(owner, spender, U256::MAX).unwrap();

            b.iter(|| {
                let token = black_box(&mut token);
                let spender = black_box(spender);
                token.transfer_from(spender, owner, recipient, U256::ONE).unwrap();
            });
        });
    });

    c.bench_function("base_token_transfer_with_memo", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let from = BaseTokenBenchSetup::initial_supply_recipient();
            let to = Address::repeat_byte(0x02);
            let memo = B256::repeat_byte(0x42);
            let mut token = BaseTokenBenchSetup::create_token(
                ctx,
                B256::repeat_byte(0x11),
                U256::from(u128::MAX),
            );

            b.iter(|| {
                let token = black_box(&mut token);
                let from = black_box(from);
                token.transfer_with_memo(from, to, U256::ONE, memo).unwrap();
            });
        });
    });

    c.bench_function("base_token_transfer_from_with_memo", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let owner = BaseTokenBenchSetup::initial_supply_recipient();
            let spender = Address::repeat_byte(0x02);
            let recipient = Address::repeat_byte(0x03);
            let memo = B256::repeat_byte(0x43);
            let mut token = BaseTokenBenchSetup::create_token(
                ctx,
                B256::repeat_byte(0x12),
                U256::from(u128::MAX),
            );
            token.approve(owner, spender, U256::MAX).unwrap();

            b.iter(|| {
                let token = black_box(&mut token);
                let spender = black_box(spender);
                token.transfer_from_with_memo(spender, owner, recipient, U256::ONE, memo).unwrap();
            });
        });
    });

    c.bench_function("base_token_pause", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let admin = BaseTokenBenchSetup::admin();
            let mut token =
                BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x13), U256::ZERO);

            b.iter(|| {
                let token = black_box(&mut token);
                let admin = black_box(admin);
                token.pause(admin, vec![IB20::PausableFeature::TRANSFER], true).unwrap();
            });
        });
    });

    c.bench_function("base_token_unpause", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let admin = BaseTokenBenchSetup::admin();
            let mut token =
                BaseTokenBenchSetup::create_token(ctx, B256::repeat_byte(0x14), U256::ZERO);
            token.pause(admin, vec![IB20::PausableFeature::TRANSFER], true).unwrap();

            b.iter(|| {
                let token = black_box(&mut token);
                let admin = black_box(admin);
                token.unpause(admin, vec![IB20::PausableFeature::TRANSFER], true).unwrap();
            });
        });
    });

    c.bench_function("base_token_set_supply_cap", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let admin = BaseTokenBenchSetup::admin();
            let mut token = BaseTokenBenchSetup::create_token(
                ctx,
                B256::repeat_byte(0x15),
                U256::from(1_000u64),
            );

            b.iter(|| {
                let token = black_box(&mut token);
                let admin = black_box(admin);
                token.set_supply_cap(admin, U256::from(10_000u64), true).unwrap();
            });
        });
    });
}

fn base_token_factory_mutate(c: &mut Criterion) {
    c.bench_function("base_token_factory_create_b20", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let caller = BaseTokenBenchSetup::caller();
            let mut counter = 0u64;

            b.iter(|| {
                counter += 1;
                let salt = B256::from(U256::from(counter));
                let params = BaseTokenBenchSetup::token_params("FactoryToken", "FACT");
                let token = BaseTokenBenchSetup::create_b20(ctx, caller, params, salt, U256::ZERO);
                black_box(token);
            });
        });
    });
}

fn base_token_factory_view(c: &mut Criterion) {
    c.bench_function("base_token_factory_predict_b20_address", |b| {
        let caller = BaseTokenBenchSetup::caller();
        let salt = B256::repeat_byte(0x21);

        b.iter(|| {
            let caller = black_box(caller);
            let salt = black_box(salt);
            let result = TokenVariant::B20.compute_address(caller, salt);
            black_box(result);
        });
    });

    c.bench_function("base_token_factory_predict_stablecoin_address", |b| {
        let caller = BaseTokenBenchSetup::caller();
        let salt = B256::repeat_byte(0x22);

        b.iter(|| {
            let caller = black_box(caller);
            let salt = black_box(salt);
            let result = TokenVariant::Stablecoin.compute_address(caller, salt);
            black_box(result);
        });
    });

    c.bench_function("base_token_factory_predict_security_address", |b| {
        let caller = BaseTokenBenchSetup::caller();
        let salt = B256::repeat_byte(0x23);

        b.iter(|| {
            let caller = black_box(caller);
            let salt = black_box(salt);
            let result = TokenVariant::Security.compute_address(caller, salt);
            black_box(result);
        });
    });

    c.bench_function("base_token_factory_is_b20", |b| {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| {
            let params = BaseTokenBenchSetup::token_params("FactoryToken", "FACT");
            let token_address = BaseTokenBenchSetup::create_b20(
                ctx,
                BaseTokenBenchSetup::caller(),
                params,
                B256::repeat_byte(0x24),
                U256::ZERO,
            );
            let factory = TokenFactoryStorage::new(ctx);

            b.iter(|| {
                let factory = black_box(&factory);
                let token_address = black_box(token_address);
                let result = factory.is_b20(token_address).unwrap();
                black_box(result);
            });
        });
    });

    c.bench_function("base_token_factory_get_token_variant", |b| {
        let (token_address, _) = TokenVariant::B20
            .compute_address(BaseTokenBenchSetup::caller(), B256::repeat_byte(0x25));

        b.iter(|| {
            let token_address = black_box(token_address);
            let result = TokenVariant::from_address(token_address);
            black_box(result);
        });
    });
}

criterion_group!(
    benches,
    base_token_metadata,
    base_token_view,
    base_token_mutate,
    base_token_factory_mutate,
    base_token_factory_view,
);
criterion_main!(benches);
