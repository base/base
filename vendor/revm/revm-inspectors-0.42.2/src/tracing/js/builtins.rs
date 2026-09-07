//! Builtin functions

use alloc::{borrow::Cow, format, string::ToString, vec::Vec};
use alloy_primitives::{hex, map::HashSet, Address, FixedBytes, B256, U256};
use boa_engine::{
    builtins::{array_buffer::ArrayBuffer, typed_array::TypedArray},
    js_string,
    object::builtins::{JsArray, JsArrayBuffer, JsTypedArray, JsUint8Array},
    property::Attribute,
    Context, JsArgs, JsBigInt, JsError, JsNativeError, JsResult, JsString, JsValue, NativeFunction,
    Source,
};
use boa_gc::{empty_trace, Finalize, Trace};
use core::borrow::Borrow;

/// Converts the given `JsValue` to a `serde_json::Value`.
///
/// This first attempts to use the built-in `JSON.stringify` function to convert the value to a JSON
///
/// If that fails it uses boa's to_json function to convert the value to a JSON object
///
/// We use `JSON.stringify` so that `toJSON` properties are used when converting the value to JSON,
/// this ensures the `bigint` is serialized properly.
pub(crate) fn to_serde_value(val: JsValue, ctx: &mut Context) -> JsResult<serde_json::Value> {
    if let Ok(json) = json_stringify(val.clone(), ctx) {
        let json = json.to_std_string().map_err(|err| {
            JsError::from_native(
                JsNativeError::error()
                    .with_message(format!("failed to convert JSON to string: {err}")),
            )
        })?;
        serde_json::from_str(&json).map_err(|err| {
            JsError::from_native(
                JsNativeError::error().with_message(format!("failed to parse JSON: {err}")),
            )
        })
    } else {
        val.to_json(ctx)?.ok_or_else(|| {
            JsError::from_native(
                JsNativeError::error().with_message("failed to convert JsValue to JSON"),
            )
        })
    }
}

/// Attempts to use the global `JSON` object to stringify the given value.
pub(crate) fn json_stringify(val: JsValue, ctx: &mut Context) -> JsResult<JsString> {
    let json = ctx.global_object().get(js_string!("JSON"), ctx)?;
    let json_obj = json.as_object().ok_or_else(|| {
        JsError::from_native(JsNativeError::typ().with_message("JSON is not an object"))
    })?;

    let stringify = json_obj.get(js_string!("stringify"), ctx)?;

    let stringify = stringify.as_callable().ok_or_else(|| {
        JsError::from_native(JsNativeError::typ().with_message("JSON.stringify is not callable"))
    })?;
    let res = stringify.call(&json, &[val], ctx)?;
    res.to_string(ctx)
}

/// Registers all the builtin functions.
///
/// Note: this does not register the `isPrecompiled` builtin, as this requires the precompile
/// addresses, see [PrecompileList::register_callable].
pub(crate) fn register_builtins(ctx: &mut Context) -> JsResult<()> {
    let big_int = ctx.global_object().get(js_string!("BigInt"), ctx)?;
    // Add toJSON method and geth-compatible polyfill shims to BigInt prototype.
    //
    // Geth's JS tracer uses the BigInteger.js polyfill (peterolson/BigInteger.js) which exposes
    // a global `bigInt` (camelCase) function returning objects with methods like `.equals()`,
    // `.toJSNumber()`, `.plus()`, `.minus()`, etc. Reth uses Boa's native BigInt which lacks
    // these methods. We add shims so geth-compatible tracers (including geth's built-in
    // call_tracer_legacy.js) work unmodified.
    //
    // See: https://github.com/ethereum/go-ethereum/blob/master/eth/tracers/js/bigint.go
    ctx.eval(Source::from_bytes(
        br#"
BigInt.prototype.toJSON = function() { return this.toString(); };
BigInt.prototype.equals = function(other) { return this == other; };
BigInt.prototype.toJSNumber = function() { return Number(this); };
BigInt.prototype.plus = function(other) { return this + BigInt(other); };
BigInt.prototype.minus = function(other) { return this - BigInt(other); };
"#,
    ))?;
    // Create global 'bigint' alias for native BigInt constructor (lowercase for compatibility)
    ctx.register_global_property(js_string!("bigint"), big_int.clone(), Attribute::all())?;
    // Create global 'bigInt' alias (camelCase) for geth BigInteger.js polyfill compatibility.
    // Geth's goja engine runs `var bigInt = function(){...}()` at global scope, making `bigInt`
    // the standard way to construct big integers in geth JS tracers.
    ctx.register_global_property(js_string!("bigInt"), big_int, Attribute::all())?;
    ctx.register_global_builtin_callable(
        js_string!("toHex"),
        1,
        NativeFunction::from_fn_ptr(to_hex),
    )?;
    ctx.register_global_callable(js_string!("toWord"), 1, NativeFunction::from_fn_ptr(to_word))?;
    ctx.register_global_callable(
        js_string!("toAddress"),
        1,
        NativeFunction::from_fn_ptr(to_address),
    )?;
    ctx.register_global_callable(
        js_string!("toContract"),
        2,
        NativeFunction::from_fn_ptr(to_contract),
    )?;
    ctx.register_global_callable(
        js_string!("toContract2"),
        3,
        NativeFunction::from_fn_ptr(to_contract2),
    )?;
    ctx.register_global_callable(js_string!("slice"), 3, NativeFunction::from_fn_ptr(slice))?;

    Ok(())
}

/// Converts an array, hex string or Uint8Array to a byte array.
pub(crate) fn bytes_from_value(val: JsValue, context: &mut Context) -> JsResult<Vec<u8>> {
    if let Some(obj) = val.as_object() {
        if obj.is::<TypedArray>() {
            let array: JsTypedArray = JsTypedArray::from_object(obj)?;
            let len = array.length(context)?;
            let mut buf = Vec::with_capacity(len);
            for i in 0..len {
                let val = array.get(i, context)?;
                buf.push(val.to_number(context)? as u8);
            }
            return Ok(buf);
        } else if obj.is::<ArrayBuffer>() {
            let buf = JsArrayBuffer::from_object(obj)?;
            let buf = buf.data().map(|data| data.to_vec()).ok_or_else(|| {
                JsNativeError::typ().with_message("ArrayBuffer was already detached")
            })?;
            return Ok(buf);
        } else if obj.is::<JsString>() {
            let js_string = obj.downcast_ref::<JsString>().unwrap();
            return hex_decode_js_string(js_string.borrow());
        } else if obj.is_array() {
            let array = JsArray::from_object(obj)?;
            let len = array.length(context)?;
            let mut buf = Vec::with_capacity(len as usize);
            for i in 0..len {
                let val = array.get(i, context)?;
                buf.push(val.to_number(context)? as u8);
            }
            return Ok(buf);
        }
    }

    if let Some(js_string) = val.as_string() {
        return hex_decode_js_string(&js_string);
    }

    Err(JsError::from_native(
        JsNativeError::typ().with_message(format!("invalid buffer type: {}", val.type_of())),
    ))
}

/// Create a new [JsUint8Array] array buffer from the address' bytes.
pub(crate) fn address_to_uint8_array(
    addr: Address,
    context: &mut Context,
) -> JsResult<JsUint8Array> {
    JsUint8Array::from_iter(addr, context)
}

/// Create a new [JsUint8Array] array buffer from the address' bytes.
pub(crate) fn address_to_uint8_array_value(
    addr: Address,
    context: &mut Context,
) -> JsResult<JsValue> {
    address_to_uint8_array(addr, context).map(Into::into)
}

/// Create a new [JsUint8Array] from byte block.
pub(crate) fn to_uint8_array<I>(bytes: I, context: &mut Context) -> JsResult<JsUint8Array>
where
    I: IntoIterator<Item = u8>,
{
    JsUint8Array::from_iter(bytes, context)
}

/// Create a new [JsUint8Array] object from byte block.
pub(crate) fn to_uint8_array_value<I>(bytes: I, context: &mut Context) -> JsResult<JsValue>
where
    I: IntoIterator<Item = u8>,
{
    to_uint8_array(bytes, context).map(Into::into)
}

/// Converts a slice of bytes to an address.
///
/// See [`bytes_to_fb`] for more information.
pub(crate) fn bytes_to_address(bytes: &[u8]) -> Address {
    Address(bytes_to_fb(bytes))
}

/// Converts a slice of bytes to a 32-byte fixed-size array.
///
/// See [`bytes_to_fb`] for more information.
pub(crate) fn bytes_to_b256(bytes: &[u8]) -> B256 {
    bytes_to_fb(bytes)
}

/// Converts a slice of bytes to a fixed-size array.
///
/// If the slice is larger than the array size, it will be trimmed from the left.
pub(crate) fn bytes_to_fb<const N: usize>(mut bytes: &[u8]) -> FixedBytes<N> {
    if bytes.len() > N {
        bytes = &bytes[bytes.len() - N..];
    }
    FixedBytes::left_padding_from(bytes)
}

/// Converts a U256 to a Boa bigint value.
pub(crate) fn to_bigint(value: U256) -> JsResult<JsValue> {
    JsBigInt::from_string(&value.to_string()).map(Into::into).ok_or_else(|| {
        JsError::from_native(
            JsNativeError::error().with_message("failed to convert U256 to BigInt"),
        )
    })
}

/// Compute the address of a contract created using CREATE2.
///
/// Arguments:
/// 1. creator: The address of the contract creator
/// 2. salt: A 32-byte salt value
/// 3. initcode: The contract's initialization code
///
/// Returns: The computed contract address as an ArrayBuffer
pub(crate) fn to_contract2(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Extract the sender's address, salt and initcode from the arguments
    let from = args.get_or_undefined(0).clone();
    let salt = match args.get_or_undefined(1).to_string(ctx) {
        Ok(js_string) => {
            let buf = hex_decode_js_string(&js_string)?;
            bytes_to_b256(&buf)
        }
        Err(_) => {
            return Err(JsError::from_native(
                JsNativeError::typ().with_message("invalid salt type"),
            ))
        }
    };
    let initcode = args.get_or_undefined(2).clone();

    // Convert the sender's address to a byte buffer and then to an Address
    let buf = bytes_from_value(from, ctx)?;
    let addr = bytes_to_address(&buf);

    // Convert the initcode to a byte buffer
    let code_buf = bytes_from_value(initcode, ctx)?;

    // Compute the contract address
    let contract_addr = addr.create2_from_code(salt, code_buf);

    // Convert the contract address to a byte buffer and return it as an ArrayBuffer
    address_to_uint8_array_value(contract_addr, ctx)
}

/// Compute the address of a contract created by the sender with the given nonce.
///
/// Arguments:
/// 1. from: The address of the contract creator
/// 2. nonce: The creator's transaction count (optional, none is 0)
///
/// Returns: The computed contract address as an ArrayBuffer
pub(crate) fn to_contract(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    // Extract the sender's address and nonce from the arguments
    let from = args.get_or_undefined(0).clone();
    let nonce = args.get_or_undefined(1).to_number(ctx)? as u64;

    // Convert the sender's address to a byte buffer and then to an Address
    let buf = bytes_from_value(from, ctx)?;
    let addr = bytes_to_address(&buf);

    // Compute the contract address
    let contract_addr = addr.create(nonce);

    // Convert the contract address to a byte buffer and return it as an ArrayBuffer
    address_to_uint8_array_value(contract_addr, ctx)
}

/// Converts a buffer type to an address
pub(crate) fn to_address(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let val = args.get_or_undefined(0).clone();
    let buf = bytes_from_value(val, ctx)?;
    let address = bytes_to_address(&buf);
    address_to_uint8_array_value(address, ctx)
}

/// Converts a buffer type to a word
pub(crate) fn to_word(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let val = args.get_or_undefined(0).clone();
    let buf = bytes_from_value(val, ctx)?;
    let hash = bytes_to_b256(&buf);
    to_uint8_array_value(hash, ctx)
}

/// Converts a buffer type to a hex string
pub(crate) fn to_hex(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let val = args.get_or_undefined(0).clone();
    let buf = bytes_from_value(val, ctx)?;
    let s = js_string!(hex::encode_prefixed(buf));
    Ok(JsValue::from(s))
}

/// Decodes a hex decoded js-string
fn hex_decode_js_string(js_string: &JsString) -> JsResult<Vec<u8>> {
    match js_string.to_std_string() {
        Ok(s) => {
            // hex decoding strings is pretty relaxed in geth reference implementation, which allows uneven hex values <https://github.com/ethereum/go-ethereum/blob/355228b011ef9a85ebc0f21e7196f892038d49f0/common/bytes.go#L33-L35>
            // <https://github.com/paradigmxyz/reth/issues/16289>
            let mut s = Cow::Borrowed(s.strip_prefix("0x").unwrap_or(s.as_str()));
            if s.as_ref().len() % 2 == 1 {
                s = Cow::Owned(format!("0{s}"));
            }

            match hex::decode(s.as_ref()) {
                Ok(data) => Ok(data),
                Err(err) => Err(JsError::from_native(
                    JsNativeError::error()
                        .with_message(format!("invalid hex string: \"{s}\": {err}",)),
                )),
            }
        }
        Err(err) => Err(JsError::from_native(
            JsNativeError::error()
                .with_message(format!("invalid utf8 string {js_string:?}: {err}",)),
        )),
    }
}

/// Returns a slice of the given value.
pub(crate) fn slice(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let val = args.get_or_undefined(0).clone();

    let buf = bytes_from_value(val, ctx)?;
    let start = args.get_or_undefined(1).to_numeric_number(ctx)? as usize;
    let end = args.get_or_undefined(2).to_numeric_number(ctx)? as usize;

    if start > end || end > buf.len() {
        Err(JsError::from_native(JsNativeError::error().with_message(format!(
            "Tracer accessed out of bound memory: available {}, start {}, end {}",
            buf.len(),
            start,
            end
        ))))
    } else {
        to_uint8_array_value(buf[start..end].iter().copied(), ctx)
    }
}

/// A container for all precompile addresses used for the `isPrecompiled` global callable.
#[derive(Clone, Debug)]
pub(crate) struct PrecompileList(pub(crate) HashSet<Address>);

impl PrecompileList {
    /// Registers the global callable `isPrecompiled`
    pub(crate) fn register_callable(self, ctx: &mut Context) -> JsResult<()> {
        let is_precompiled = NativeFunction::from_copy_closure_with_captures(
            move |_this, args, precompiles, ctx| {
                let val = args.get_or_undefined(0).clone();
                let buf = bytes_from_value(val, ctx)?;
                let addr = bytes_to_address(&buf);
                Ok(precompiles.0.contains(&addr).into())
            },
            self,
        );

        ctx.register_global_callable(js_string!("isPrecompiled"), 1, is_precompiled)?;

        Ok(())
    }
}

impl Finalize for PrecompileList {}

unsafe impl Trace for PrecompileList {
    empty_trace!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_install_bigint() {
        let mut ctx = Context::default();
        register_builtins(&mut ctx).unwrap();

        // Test that 'bigint' alias exists and works
        let bigint = ctx.global_object().get(js_string!("bigint"), &mut ctx).unwrap();
        assert!(bigint.is_callable());

        let value = JsValue::from(js_string!("100"));
        let result =
            bigint.as_callable().unwrap().call(&JsValue::undefined(), &[value], &mut ctx).unwrap();
        assert!(result.is_bigint());
        assert_eq!(result.to_string(&mut ctx).unwrap().to_std_string().unwrap(), "100");
    }

    #[test]
    fn test_to_bigint_function() {
        let mut ctx = Context::default();
        register_builtins(&mut ctx).unwrap();

        // Test various U256 values through to_bigint
        let test_cases = vec![
            (U256::ZERO, "0"),
            (U256::from(1u64), "1"),
            (U256::from(42u64), "42"),
            (U256::from(u64::MAX), "18446744073709551615"),
            (
                U256::from_str_radix("123456789012345678901234567890", 10).unwrap(),
                "123456789012345678901234567890",
            ),
        ];

        for (value, expected) in test_cases {
            let result = to_bigint(value).unwrap();
            assert!(result.is_bigint(), "Result should be a bigint for value {value}");
            let result_str = result.to_string(&mut ctx).unwrap().to_std_string().unwrap();
            assert_eq!(result_str, expected, "BigInt conversion failed for {value}");
        }

        // Test that the result can be used in JavaScript operations
        let big_value = U256::from(999u64);
        let bigint_result = to_bigint(big_value).unwrap();

        // Set it as a global variable
        ctx.global_object().set(js_string!("testBigInt"), bigint_result, false, &mut ctx).unwrap();

        // Test arithmetic with it
        let arithmetic_test = ctx.eval(Source::from_bytes(b"testBigInt + BigInt(1)")).unwrap();
        assert!(arithmetic_test.is_bigint());
        assert_eq!(arithmetic_test.to_string(&mut ctx).unwrap().to_std_string().unwrap(), "1000");

        // Test comparison
        let comparison_test = ctx.eval(Source::from_bytes(b"testBigInt > BigInt(500)")).unwrap();
        assert!(comparison_test.as_boolean().unwrap());
    }

    fn as_length<T>(array: T) -> usize
    where
        T: Borrow<JsValue>,
    {
        let array = array.borrow();
        let array = array.as_object().unwrap();
        let array = JsUint8Array::from_object(array.clone()).unwrap();
        array.length(&mut Context::default()).unwrap()
    }

    #[test]
    fn test_to_hex() {
        let mut ctx = Context::default();
        let value = JsValue::from(js_string!("0xdeadbeef"));
        let result = to_hex(&JsValue::undefined(), &[value], &mut ctx).unwrap();
        assert_eq!(result.to_string(&mut ctx).unwrap().to_std_string().unwrap(), "0xdeadbeef");
    }

    #[test]
    fn test_to_address() {
        let mut ctx = Context::default();
        let value = JsValue::from(js_string!("0xdeadbeef"));
        let result = to_address(&JsValue::undefined(), &[value], &mut ctx).unwrap();
        assert_eq!(as_length(&result), 20);
        assert_eq!(
            result.to_string(&mut ctx).unwrap().to_std_string().unwrap(),
            "0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,222,173,190,239"
        );
    }

    #[test]
    fn test_to_word() {
        let mut ctx = Context::default();
        let value = JsValue::from(js_string!("0xdeadbeef"));
        let result = to_word(&JsValue::undefined(), &[value], &mut ctx).unwrap();
        assert_eq!(as_length(&result), 32);
        assert_eq!(
            result.to_string(&mut ctx).unwrap().to_std_string().unwrap(),
            "0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,222,173,190,239"
        );
    }

    #[test]
    fn test_to_word_digit_string() {
        let mut ctx = Context::default();
        let value = JsValue::from(js_string!("1"));
        let result = to_word(&JsValue::undefined(), &[value], &mut ctx).unwrap();
        assert_eq!(as_length(&result), 32);
        assert_eq!(
            result.to_string(&mut ctx).unwrap().to_std_string().unwrap(),
            "0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1"
        );
    }

    #[test]
    fn test_to_contract() {
        let mut ctx = Context::default();
        let from = JsValue::from(js_string!("0xdeadbeef"));
        let nonce = JsValue::from(0);
        let result = to_contract(&JsValue::undefined(), &[from.clone(), nonce], &mut ctx).unwrap();
        assert_eq!(as_length(&result), 20);
        let addr = to_hex(&JsValue::undefined(), &[result], &mut ctx).unwrap();
        assert_eq!(
            addr.to_string(&mut ctx).unwrap().to_std_string().unwrap(),
            "0xe8279be14e9fe2ad2d8e52e42ca96fb33a813bbe",
        );

        // without nonce
        let result = to_contract(&JsValue::undefined(), &[from], &mut ctx).unwrap();
        let addr = to_hex(&JsValue::undefined(), &[result], &mut ctx).unwrap();
        assert_eq!(
            addr.to_string(&mut ctx).unwrap().to_std_string().unwrap(),
            "0xe8279be14e9fe2ad2d8e52e42ca96fb33a813bbe",
        );
    }
    #[test]
    fn test_to_contract2() {
        let mut ctx = Context::default();
        let from = JsValue::from(js_string!("0xdeadbeef"));
        let salt = JsValue::from(js_string!("0xdead4a17"));
        let code = JsValue::from(js_string!("0xdeadbeef"));
        let result = to_contract2(&JsValue::undefined(), &[from, salt, code], &mut ctx).unwrap();
        assert_eq!(as_length(&result), 20);
        let addr = to_hex(&JsValue::undefined(), &[result], &mut ctx).unwrap();
        assert_eq!(
            addr.to_string(&mut ctx).unwrap().to_std_string().unwrap(),
            "0x8a0d8a428b30200a296dfbe693310e5d6d2c64c5"
        );
    }

    #[test]
    fn test_bigint_camelcase_alias() {
        let mut ctx = Context::default();
        register_builtins(&mut ctx).unwrap();

        let bigint = ctx.global_object().get(js_string!("bigInt"), &mut ctx).unwrap();
        assert!(bigint.is_callable());

        let result = ctx.eval(Source::from_bytes(b"bigInt(42).toString()")).unwrap();
        assert_eq!(result.to_string(&mut ctx).unwrap().to_std_string().unwrap(), "42");

        let result = ctx.eval(Source::from_bytes(b"bigInt('100').toString(16)")).unwrap();
        assert_eq!(result.to_string(&mut ctx).unwrap().to_std_string().unwrap(), "64");
    }

    #[test]
    fn test_bigint_equals_shim() {
        let mut ctx = Context::default();
        register_builtins(&mut ctx).unwrap();

        let result = ctx.eval(Source::from_bytes(b"bigInt(1).equals(bigInt(1))")).unwrap();
        assert!(result.as_boolean().unwrap());

        let result = ctx.eval(Source::from_bytes(b"bigInt(1).equals(bigInt(2))")).unwrap();
        assert!(!result.as_boolean().unwrap());

        let result = ctx.eval(Source::from_bytes(b"BigInt(0).equals(0)")).unwrap();
        assert!(result.as_boolean().unwrap());
    }

    #[test]
    fn test_bigint_to_js_number_shim() {
        let mut ctx = Context::default();
        register_builtins(&mut ctx).unwrap();

        let result = ctx.eval(Source::from_bytes(b"bigInt(42).toJSNumber()")).unwrap();
        assert_eq!(result.to_number(&mut ctx).unwrap(), 42.0);

        let result = ctx.eval(Source::from_bytes(b"typeof bigInt(42).toJSNumber()")).unwrap();
        assert_eq!(result.to_string(&mut ctx).unwrap().to_std_string().unwrap(), "number");
    }

    #[test]
    fn test_bigint_plus_minus_shim() {
        let mut ctx = Context::default();
        register_builtins(&mut ctx).unwrap();

        let result = ctx.eval(Source::from_bytes(b"bigInt(1).plus(bigInt(2)).toString()")).unwrap();
        assert_eq!(result.to_string(&mut ctx).unwrap().to_std_string().unwrap(), "3");

        let result =
            ctx.eval(Source::from_bytes(b"bigInt(10).minus(bigInt(3)).toString()")).unwrap();
        assert_eq!(result.to_string(&mut ctx).unwrap().to_std_string().unwrap(), "7");
    }

    #[test]
    fn test_bigint_geth_call_tracer_pattern() {
        let mut ctx = Context::default();
        register_builtins(&mut ctx).unwrap();

        // Simulates geth's call_tracer_legacy.js patterns:
        // bigInt(gasIn - gasCost - gas).toString(16)
        let result =
            ctx.eval(Source::from_bytes(b"'0x' + bigInt(1000 - 200 - 100).toString(16)")).unwrap();
        assert_eq!(result.to_string(&mut ctx).unwrap().to_std_string().unwrap(), "0x2bc");

        // !ret.equals(0) pattern
        let result = ctx.eval(Source::from_bytes(b"var ret = bigInt(1); !ret.equals(0)")).unwrap();
        assert!(result.as_boolean().unwrap());

        let result = ctx.eval(Source::from_bytes(b"var ret = bigInt(0); !ret.equals(0)")).unwrap();
        assert!(!result.as_boolean().unwrap());
    }
}
