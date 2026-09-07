#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{quote, quote_spanned};
use syn_3_0_2::{
    AngleBracketedGenericArguments, FnArg, GenericArgument, GenericParam, Ident, ItemFn, LitStr,
    Pat, PatIdent, PatSlice, PathArguments, ReturnType, Stmt, Token, Type, TypeInfer,
    TypeParamBound, TypePath, TypeSlice,
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
    spanned::Spanned,
};

/// Generates an `Instruction` impl for an instruction function definition.
///
/// ## Parameters
///
/// - `cx: _`: Optional instruction context. It exposes `pc`, `gas`, and `state`, which are needed
///   for immediates, memory, host access, dynamic gas, active version data, and message or
///   transaction state.
/// - `[a, b]: [Word]`: Automatic stack inputs. The macro checks stack bounds and binds one
///   reference per non-`_` pattern. Inputs that overlap output slots are shared reborrows of the
///   output slot; other inputs are shared references. Use reference patterns like `[&x]: [Word]` to
///   copy.
/// - `-> out`: Automatic stack output. The output name is a mutable `Word` slot. Multiple outputs
///   can be written as `-> Result<out1, out2>` for fallible instructions.
/// - `-> Result`: Fallible instruction with no automatic outputs. The body must evaluate to
///   `evm2::interpreter::Result`, so `?` can be used to return an `InstrStop`.
/// - `-> Result<out>`: Fallible instruction with automatic outputs. The body gets mutable output
///   slots and may use `?`; no explicit final `Ok(())` is needed.
///
/// ## Attributes
///
/// - `#[instruction(no_stack_preamble)]`: Disables automatic stack input and output handling. The
///   body receives a `stack` local and is responsible for all stack reads, writes, and bounds
///   checks.
/// - `#[instruction(dynamic_gas)]`: Exposes `cx.gas` and marks the instruction as needing access to
///   mutable gas state.
/// - `#[instruction(EvmTypes = CustomTypes)]`: Implements the instruction for a concrete EVM type
///   family instead of generating a generic implementation.
/// - `#[instruction(EvmTypes: CustomTypesTrait)]`: Adds a trait bound to the generated generic EVM
///   types parameter.
/// - `#[instruction(EvmTypes<Host<'static>: CustomHostTrait>)]`: Adds associated-type constraints
///   to the generated generic EVM types implementation.
///
/// ## Examples
///
/// ### Full Signature
///
/// ```ignore
/// use evm2::interpreter::{Result, Word};
/// use evm2_macros::instruction;
///
/// #[instruction]
/// fn opcode_name(cx: _, [a, b, c]: [Word]) -> Result<out> {
///     cx.gas.spend(3)?;
///     *out = a.wrapping_add(*b).wrapping_add(*c);
/// }
/// ```
///
/// ### Stack only
///
/// Stack-only instructions can omit `cx: _` and `Result`:
///
/// ```ignore
/// use evm2::interpreter::Word;
/// use evm2_macros::instruction;
///
/// #[instruction]
/// fn add([a, b]: [Word]) -> out {
///     *out = a.wrapping_add(*b);
/// }
/// ```
///
/// ### No stack preamble
///
/// ```ignore
/// use evm2::interpreter::{Result, Word};
/// use evm2_macros::instruction;
///
/// #[instruction(no_stack_preamble)]
/// fn no_stack_preamble_opcode(cx: _) -> Result {
///     cx.gas.spend(2)?;
///     stack.push(Word::ZERO)
/// }
/// ```
#[proc_macro_attribute]
pub fn instruction(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args =
        parse_macro_input!(attr with Punctuated::<InstructionAttr, Token![,]>::parse_terminated);
    let attrs = match InstructionAttrs::parse(args) {
        Ok(attrs) => attrs,
        Err(err) => return err.to_compile_error().into(),
    };
    let input = parse_macro_input!(item as ItemFn);
    expand_instruction(attrs, input).into()
}

enum InstructionAttr {
    Flag(Ident),
    EvmTypesConcrete { span: proc_macro2::Span, evm_types: Type },
    EvmTypesBounds { span: proc_macro2::Span, bounds: Punctuated<TypeParamBound, Token![+]> },
    EvmTypesArgs { span: proc_macro2::Span, args: AngleBracketedGenericArguments },
}

impl Parse for InstructionAttr {
    fn parse(input: ParseStream<'_>) -> syn_3_0_2::Result<Self> {
        let ident = input.parse::<Ident>()?;
        if ident == "EvmTypes" || ident == "evm_types" {
            let span = ident.span();
            if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                Ok(Self::EvmTypesConcrete { span, evm_types: input.parse()? })
            } else if input.peek(Token![:]) {
                input.parse::<Token![:]>()?;
                Ok(Self::EvmTypesBounds {
                    span,
                    bounds: Punctuated::<TypeParamBound, Token![+]>::parse_separated_nonempty(
                        input,
                    )?,
                })
            } else if input.peek(Token![<]) {
                Ok(Self::EvmTypesArgs { span, args: input.parse()? })
            } else {
                Err(syn_3_0_2::Error::new_spanned(
                    ident,
                    "expected `=`, `:`, or `<...>` after `EvmTypes`",
                ))
            }
        } else {
            Ok(Self::Flag(ident))
        }
    }
}

#[derive(Clone, Default)]
struct InstructionAttrs {
    no_stack_preamble: bool,
    dynamic_gas: bool,
    evm_types: Option<Type>,
    evm_types_span: Option<proc_macro2::Span>,
    evm_types_bounds: Vec<Punctuated<TypeParamBound, Token![+]>>,
    evm_types_args: Option<AngleBracketedGenericArguments>,
}

impl InstructionAttrs {
    fn parse(args: Punctuated<InstructionAttr, Token![,]>) -> syn_3_0_2::Result<Self> {
        let mut attrs = Self::default();
        for arg in args {
            match arg {
                InstructionAttr::Flag(arg) if arg == "no_stack_preamble" => {
                    attrs.no_stack_preamble = true;
                }
                InstructionAttr::Flag(arg) if arg == "dynamic_gas" => {
                    attrs.dynamic_gas = true;
                }
                InstructionAttr::Flag(arg) => {
                    return Err(syn_3_0_2::Error::new_spanned(
                        arg,
                        "unsupported #[instruction] argument",
                    ));
                }
                InstructionAttr::EvmTypesConcrete { span, evm_types } => {
                    if attrs.evm_types.replace(evm_types).is_some() {
                        return Err(syn_3_0_2::Error::new(span, "duplicate `EvmTypes` argument"));
                    }
                    attrs.evm_types_span = Some(span);
                }
                InstructionAttr::EvmTypesBounds { span, bounds } => {
                    attrs.evm_types_span.get_or_insert(span);
                    attrs.evm_types_bounds.push(bounds);
                }
                InstructionAttr::EvmTypesArgs { span, args } => {
                    if attrs.evm_types_args.replace(args).is_some() {
                        return Err(syn_3_0_2::Error::new(
                            span,
                            "duplicate `EvmTypes<...>` argument",
                        ));
                    }
                    attrs.evm_types_span.get_or_insert(span);
                }
            }
        }
        if attrs.evm_types.is_some()
            && (!attrs.evm_types_bounds.is_empty() || attrs.evm_types_args.is_some())
        {
            return Err(syn_3_0_2::Error::new(
                attrs.evm_types_span.unwrap_or_else(proc_macro2::Span::call_site),
                "`EvmTypes = ...` cannot be combined with generic `EvmTypes` bounds",
            ));
        }
        Ok(attrs)
    }
}

fn expand_instruction(instruction_attrs: InstructionAttrs, input: ItemFn) -> TokenStream2 {
    let fn_attrs = input.attrs;
    let vis = input.vis;
    let sig = input.sig;
    let ident = sig.ident;
    let asm_comment = LitStr::new(&ident.to_string(), ident.span());
    let generics = sig.generics;
    let impl_params = generics.params.clone();
    let evm_types_span = instruction_attrs.evm_types_span.unwrap_or_else(|| ident.span());
    let evm_types_ident = Ident::new("__Evm2T", evm_types_span);
    let evm_types = instruction_attrs
        .evm_types
        .as_ref()
        .map(|ty| quote! { #ty })
        .unwrap_or_else(|| quote! { #evm_types_ident });
    let struct_generics = match (&instruction_attrs.evm_types, impl_params.is_empty()) {
        (Some(_), true) => quote! {},
        (Some(_), false) => quote! { <#impl_params> },
        (None, true) => quote! { <#evm_types_ident> },
        (None, false) => quote! { <#evm_types_ident, #impl_params> },
    };
    let type_params = generics.params.iter().map(generic_param_ident);
    let type_generics = if instruction_attrs.evm_types.is_some() {
        if generics.params.is_empty() {
            quote! {}
        } else {
            quote! { <#(#type_params),*> }
        }
    } else {
        quote! { <#evm_types_ident #(, #type_params)*> }
    };
    let evm_types_bound = if let Some(args) = instruction_attrs.evm_types_args {
        quote! { evm2::EvmTypesHost #args }
    } else {
        quote! { evm2::EvmTypesHost }
    };
    let evm_types_bounds = instruction_attrs.evm_types_bounds;
    let where_predicates =
        generics.where_clause.as_ref().map(|where_clause| &where_clause.predicates);
    let where_clause = if let Some(predicates) = where_predicates {
        quote! { where #evm_types: #evm_types_bound #( + #evm_types_bounds)*, #predicates }
    } else {
        quote! { where #evm_types: #evm_types_bound #( + #evm_types_bounds)* }
    };
    let (_, outputs) = parse_return(sig.output);

    let mut has_cx = false;
    let mut cx_arg = None;
    let mut inputs = Vec::new();
    for arg in sig.inputs {
        let FnArg::Typed(arg) = arg else { continue };
        if is_infer(&arg.ty) {
            let Pat::Ident(PatIdent { ident, .. }) = *arg.pat else { continue };
            has_cx = true;
            cx_arg = Some(ident);
        } else if is_word_slice(&arg.ty) {
            let Pat::Slice(PatSlice { elems, .. }) = *arg.pat else { continue };
            if let Some(rest) = elems.iter().find(|pat| matches!(pat, Pat::Rest(_))) {
                return syn_3_0_2::Error::new_spanned(
                    rest,
                    "`..` is not supported in stack inputs",
                )
                .to_compile_error();
            }
            inputs.extend(elems);
        } else {
            return syn_3_0_2::Error::new_spanned(
                arg,
                "unsupported #[instruction] argument; use `cx: _` for context or `[a, b]: [Word]` for stack inputs",
            )
            .to_compile_error();
        }
    }

    let body = body(input.block.stmts, outputs.is_empty());
    let stack_setup = if instruction_attrs.no_stack_preamble {
        None
    } else {
        Some(stack_setup(&inputs, &outputs))
    };
    let cx_setup = has_cx.then(|| {
        let cx = cx_arg.unwrap_or_else(|| Ident::new("cx", ident.span()));
        if instruction_attrs.dynamic_gas {
            quote! {
                let (__evm2_gas, __evm2_state) = unsafe {
                    evm2::interpreter::private::split_gas_state(__evm2_state)
                };
                let mut #cx = evm2::interpreter::private::GasInstructionCx::<#evm_types> {
                    pc: __evm2_pc,
                    gas: __evm2_gas,
                    state: __evm2_state,
                    _non_exhaustive: (),
                };
            }
        } else {
            quote! {
                let mut #cx = evm2::interpreter::private::InstructionCx::<#evm_types> {
                    pc: __evm2_pc,
                    state: __evm2_state,
                    _non_exhaustive: (),
                };
            }
        }
    });
    let dynamic_gas = instruction_attrs.dynamic_gas;
    quote! {
        #(#fn_attrs)*
        #[allow(non_camel_case_types)]
        #vis struct #ident #struct_generics(
            core::marker::PhantomData<fn() -> #evm_types>
        ) #where_clause;

        impl #struct_generics evm2::interpreter::private::Instruction<#evm_types> for #ident #type_generics
        #where_clause
        {
            const DYNAMIC_GAS: bool = #dynamic_gas;

            #[inline]
            fn execute(
                __evm2_pc: &mut evm2::interpreter::Pc,
                mut stack: evm2::interpreter::StackMut<'_>,
                __evm2_state: &mut evm2::interpreter::InterpreterState<'_, '_, #evm_types>,
            ) -> evm2::interpreter::Result {
                evm2::asm_comment!(#asm_comment);
                #cx_setup
                #stack_setup
                #body
            }
        }
    }
}

fn generic_param_ident(param: &GenericParam) -> TokenStream2 {
    match param {
        GenericParam::Type(param) => {
            let ident = &param.ident;
            quote! { #ident }
        }
        GenericParam::Lifetime(param) => {
            let lifetime = &param.lifetime;
            quote! { #lifetime }
        }
        GenericParam::Const(param) => {
            let ident = &param.ident;
            quote! { #ident }
        }
    }
}

const fn is_infer(ty: &Type) -> bool {
    matches!(ty, Type::Infer(TypeInfer { .. }))
}

fn is_word_slice(ty: &Type) -> bool {
    let Type::Slice(TypeSlice { elem, .. }) = ty else {
        return false;
    };
    let Type::Path(TypePath { path, .. }) = &**elem else {
        return false;
    };
    path.get_ident().is_some_and(|ident| ident == "Word")
}

fn parse_return(output: ReturnType) -> (bool, Vec<Ident>) {
    let ReturnType::Type(_, ty) = output else {
        return (false, Vec::new());
    };
    let Type::Path(TypePath { path, .. }) = *ty else {
        return (false, Vec::new());
    };
    let Some(segment) = path.segments.last() else {
        return (false, Vec::new());
    };
    if segment.ident == "Result" {
        let outputs = match &segment.arguments {
            PathArguments::AngleBracketed(AngleBracketedGenericArguments { args, .. }) => {
                args.iter().filter_map(generic_ident).collect()
            }
            _ => Vec::new(),
        };
        (true, outputs)
    } else {
        (false, vec![segment.ident.clone()])
    }
}

fn generic_ident(arg: &GenericArgument) -> Option<Ident> {
    let GenericArgument::Type(Type::Path(TypePath { path, .. })) = arg else {
        return None;
    };
    path.get_ident().cloned()
}

fn body(stmts: Vec<Stmt>, allow_final_result: bool) -> TokenStream2 {
    if allow_final_result && matches!(stmts.last(), Some(Stmt::Expr(_, None))) {
        quote! { #(#stmts)* }
    } else {
        let stmts = stmts.into_iter().map(|stmt| match stmt {
            Stmt::Expr(expr, None) => quote! { #expr; },
            stmt => quote! { #stmt },
        });
        quote! {
            #(#stmts)*
            Ok(())
        }
    }
}

fn stack_setup(inputs: &[Pat], outputs: &[Ident]) -> TokenStream2 {
    let input_count = inputs.len();
    let output_count = outputs.len();
    let stack_bindings = stack_bindings(inputs, outputs);

    quote! {
        let ptr = evm2::interpreter::private::instr_stack_setup(
            &mut stack,
            #input_count,
            #output_count,
        )?;
        #stack_bindings
    }
}

fn stack_bindings(inputs: &[Pat], outputs: &[Ident]) -> TokenStream2 {
    if outputs.len() > 1 {
        return syn_3_0_2::Error::new(
            outputs.iter().map(|i| i.span()).reduce(|a, b| a.join(b).unwrap_or(a)).unwrap(),
            "multiple outputs not supported",
        )
        .into_compile_error();
    }

    let count = inputs.len().max(outputs.len());
    let mut bindings = Vec::with_capacity(count);
    for i in 0..count {
        let input = inputs.iter().rev().nth(i);
        let output = outputs.iter().rev().nth(i);
        let binding = match (input, output) {
            (Some(Pat::Wild(_)), None) => TokenStream2::new(),
            (Some(Pat::Wild(_)), Some(output)) | (None, Some(output)) => {
                output_binding(output, stack_slot_ptr(i, output.span()))
            }
            (Some(input), None) => input_binding(input, stack_slot_ptr(i, input.span())),
            (Some(input), Some(output)) => {
                if simple_ident(input).is_some_and(|input| input == *output) {
                    output_binding(output, stack_slot_ptr(i, output.span()))
                } else {
                    let output_binding = output_binding(output, stack_slot_ptr(i, output.span()));
                    let input_binding = input_reborrow_binding(input, output);
                    quote! {
                        #output_binding
                        #input_binding
                    }
                }
            }
            (None, None) => TokenStream2::new(),
        };
        bindings.push(binding);
    }

    quote! {
        #(#bindings)*
    }
}

fn output_binding(output: &Ident, ptr: TokenStream2) -> TokenStream2 {
    let span = output.span();
    quote_spanned! {
        span=> let #output = unsafe { &mut *#ptr };
    }
}

fn input_binding(input: &Pat, ptr: TokenStream2) -> TokenStream2 {
    let span = input.span();
    quote_spanned! {
        span=> let #input = unsafe { &*#ptr };
    }
}

fn input_reborrow_binding(input: &Pat, output: &Ident) -> TokenStream2 {
    let span = input.span();
    quote_spanned! {
        span=> let #input = &*#output;
    }
}

fn stack_slot_ptr(i: usize, span: proc_macro2::Span) -> TokenStream2 {
    if i == 0 {
        quote_spanned! { span=> ptr }
    } else {
        quote_spanned! { span=> ptr.add(#i) }
    }
}

fn simple_ident(input: &Pat) -> Option<Ident> {
    let Pat::Ident(PatIdent { by_ref: None, mutability: None, ident, subpat: None, .. }) = input
    else {
        return None;
    };
    Some(ident.clone())
}
