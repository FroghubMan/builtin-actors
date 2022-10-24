use fvm_ipld_encoding::{BytesDe, BytesSer};
use fvm_shared::{address::Address, METHOD_SEND};

use {
    super::memory::{copy_to_memory, get_memory_region},
    crate::interpreter::address::EthAddress,
    crate::interpreter::instructions::memory::MemoryRegion,
    crate::interpreter::output::StatusCode,
    crate::interpreter::precompiles,
    crate::interpreter::stack::Stack,
    crate::interpreter::ExecutionState,
    crate::interpreter::System,
    crate::interpreter::U256,
    crate::RawBytes,
    crate::{DelegateCallParams, Method},
    fil_actors_runtime::runtime::builtins::Type,
    fil_actors_runtime::runtime::Runtime,
    fvm_ipld_blockstore::Blockstore,
    fvm_shared::econ::TokenAmount,
};

/// The kind of call-like instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallKind {
    Call,
    DelegateCall,
    StaticCall,
    CallCode,
}

pub fn calldataload(state: &mut ExecutionState) {
    let index = state.stack.pop();
    let input_len = state.input_data.len();

    state.stack.push({
        if index > U256::from(input_len) {
            U256::zero()
        } else {
            let index_usize = index.as_usize();
            let end = core::cmp::min(index_usize + 32, input_len);

            let mut data = [0; 32];
            data[..end - index_usize].copy_from_slice(&state.input_data[index_usize..end]);

            U256::from_big_endian(&data)
        }
    });
}

#[inline]
pub fn calldatasize(state: &mut ExecutionState) {
    state.stack.push(u128::try_from(state.input_data.len()).unwrap().into());
}

pub fn calldatacopy(state: &mut ExecutionState) -> Result<(), StatusCode> {
    let mem_index = state.stack.pop();
    let input_index = state.stack.pop();
    let size = state.stack.pop();

    let region = get_memory_region(&mut state.memory, mem_index, size)
        .map_err(|_| StatusCode::InvalidMemoryAccess)?;

    if let Some(region) = &region {
        let input_len = U256::from(state.input_data.len());
        let src = core::cmp::min(input_len, input_index);
        let copy_size = core::cmp::min(size, input_len - src).as_usize();
        let src = src.as_usize();

        if copy_size > 0 {
            state.memory[region.offset..region.offset + copy_size]
                .copy_from_slice(&state.input_data[src..src + copy_size]);
        }

        if region.size.get() > copy_size {
            state.memory[region.offset + copy_size..region.offset + region.size.get()].fill(0);
        }
    }

    Ok(())
}

#[inline]
pub fn codesize(stack: &mut Stack, code: &[u8]) {
    stack.push(U256::from(code.len()))
}

pub fn codecopy(state: &mut ExecutionState, code: &[u8]) -> Result<(), StatusCode> {
    let mem_index = state.stack.pop();
    let input_index = state.stack.pop();
    let size = state.stack.pop();

    let region = get_memory_region(&mut state.memory, mem_index, size)
        .map_err(|_| StatusCode::InvalidMemoryAccess)?;

    if let Some(region) = region {
        let src = core::cmp::min(U256::from(code.len()), input_index).as_usize();
        let copy_size = core::cmp::min(region.size.get(), code.len() - src);

        if copy_size > 0 {
            state.memory[region.offset..region.offset + copy_size]
                .copy_from_slice(&code[src..src + copy_size]);
        }

        if region.size.get() > copy_size {
            state.memory[region.offset + copy_size..region.offset + region.size.get()].fill(0);
        }
    }

    Ok(())
}

pub fn call<BS: Blockstore, RT: Runtime<BS>>(
    state: &mut ExecutionState,
    system: &mut System<BS, RT>,
    kind: CallKind,
) -> Result<(), StatusCode> {
    let ExecutionState { stack, memory, .. } = state;

    // NOTE gas is currently ignored as FVM's send doesn't allow the caller to specify a gas
    //      limit (external invocation gas limit applies). This may changed in the future.
    let (_gas, dst, value, input_offset, input_size, output_offset, output_size) = match kind {
        CallKind::Call | CallKind::CallCode => (
            stack.pop(),
            stack.pop(),
            stack.pop(),
            stack.pop(),
            stack.pop(),
            stack.pop(),
            stack.pop(),
        ),

        CallKind::DelegateCall | CallKind::StaticCall => (
            stack.pop(),
            stack.pop(),
            U256::from(0),
            stack.pop(),
            stack.pop(),
            stack.pop(),
            stack.pop(),
        ),
    };

    if system.readonly && value > U256::zero() {
        // non-zero sends are side-effects and hence a static mode violation
        return Err(StatusCode::StaticModeViolation);
    }

    let input_region = get_memory_region(memory, input_offset, input_size)
        .map_err(|_| StatusCode::InvalidMemoryAccess)?;

    state.return_data = {
        // ref to memory is dropped after calling so we can mutate it on output later
        let input_data = if let Some(MemoryRegion { offset, size }) = input_region {
            &memory[offset..][..size.get()]
        } else {
            &[]
        };

        if precompiles::Precompiles::<BS, RT>::is_precompile(&dst) {
            precompiles::Precompiles::call_precompile(system.rt, dst, input_data)
                .map_err(|_| StatusCode::PrecompileFailure)?
        } else {
            let call_result = match kind {
                CallKind::Call | CallKind::StaticCall => {
                    let dst_addr: EthAddress = dst.try_into()?;
                    let dst_addr: Address = dst_addr.try_into()?;

                    // Special casing for account/embryo/non-existent actors: we just do a SEND (method 0)
                    // which allows us to transfer funds (and create embryos)
                    let target_actor_code = system
                        .rt
                        .resolve_address(&dst_addr)
                        .and_then(|actor_id| system.rt.get_actor_code_cid(&actor_id));
                    let target_actor_type = target_actor_code
                        .as_ref()
                        .and_then(|cid| system.rt.resolve_builtin_actor_type(cid));
                    let actor_exists = target_actor_code.is_some();

                    if !actor_exists && value.is_zero() {
                        // If the actor doesn't exist and we're not sending value, return with
                        // "success". The EVM only auto-creates actors when sending value.
                        //
                        // NOTE: this will also apply if we're in read-only mode, because we can't
                        // send value in read-only mode anyways.
                        Ok(RawBytes::default())
                    } else {
                        let method = if !actor_exists
                            || matches!(target_actor_type, Some(Type::Embryo | Type::Account))
                        {
                            // If the target actor doesn't exist or is an account or an embryo,
                            // switch to a basic "send" so the call will still work even if the
                            // target actor would reject a normal ethereum call.
                            METHOD_SEND
                        } else if system.readonly || kind == CallKind::StaticCall {
                            // Invoke, preserving read-only mode.
                            Method::InvokeContractReadOnly as u64
                        } else {
                            // Otherwise, invoke normally.
                            Method::InvokeContract as u64
                        };
                        system.send(
                            &dst_addr,
                            method,
                            // TODO: support IPLD codecs #758
                            RawBytes::serialize(BytesSer(input_data))?,
                            TokenAmount::from(&value),
                        )
                    }
                }
                CallKind::DelegateCall => {
                    // first invoke GetBytecode to get the code CID from the target
                    let code = crate::interpreter::instructions::ext::get_evm_bytecode_cid(
                        system.rt, dst,
                    )?;

                    // and then invoke self with delegate; readonly context is sticky
                    let params = DelegateCallParams {
                        code,
                        input: input_data.to_vec(),
                        readonly: system.readonly,
                    };
                    system.send(
                        &system.rt.message().receiver(),
                        Method::InvokeContractDelegate as u64,
                        RawBytes::serialize(&params)?,
                        TokenAmount::from(&value),
                    )
                }

                CallKind::CallCode => {
                    todo!()
                }
            };
            match call_result {
                Ok(result) => {
                    // Support the "empty" result. We often use this to mean "returned nothing" and
                    // it's important to support, e.g., sending to accounts.
                    if result.is_empty() {
                        Vec::new()
                    } else {
                        // TODO: support IPLD codecs #758
                        let BytesDe(result) = result.deserialize()?;
                        result
                    }
                }
                Err(_) => {
                    // reverted -- we don't have return data yet
                    // push failure
                    stack.push(U256::zero());
                    return Ok(());
                }
            }
        }
    }
    .into();

    // copy return data to output region if it is non-zero
    copy_to_memory(memory, output_offset, output_size, U256::zero(), &state.return_data)?;

    stack.push(U256::from(1));
    Ok(())
}

pub fn callactor<BS: Blockstore, RT: Runtime<BS>>(
    state: &mut ExecutionState,
    system: &System<BS, RT>,
) -> Result<(), StatusCode> {
    let ExecutionState { stack, memory, .. } = state;
    let rt = &*system.rt; // as immutable reference

    // TODO Until we support readonly (static) calls at the fvm level, we disallow callactor
    //      when in static mode as it is sticky and there are no guarantee of preserving the
    //      static invariant
    if system.readonly {
        return Err(StatusCode::StaticModeViolation);
    }

    // stack: GAS DEST VALUE METHODNUM INPUT-OFFSET INPUT-SIZE
    // NOTE: we don't need output-offset/output-size (which the CALL instructions have)
    //       becase these are kinda useless; we can just use RETURNDATA anyway.
    // NOTE: gas is currently ignored
    let _gas = stack.pop();
    let dst = stack.pop();
    let value = stack.pop();
    let method = stack.pop();
    let input_offset = stack.pop();
    let input_size = stack.pop();

    let input_region = get_memory_region(memory, input_offset, input_size)
        .map_err(|_| StatusCode::InvalidMemoryAccess)?;

    let result = {
        let dst_addr: EthAddress = dst.try_into()?;
        let dst_addr: Address = dst_addr.try_into()?;

        if method.bits() > 64 {
            return Err(StatusCode::ArgumentOutOfRange(format!("bad method number: {}", method)));
        }
        let methodnum = method.as_u64();

        let input_data = if let Some(MemoryRegion { offset, size }) = input_region {
            &memory[offset..][..size.get()]
        } else {
            &[]
        }
        .to_vec();
        rt.send(&dst_addr, methodnum, RawBytes::from(input_data), TokenAmount::from(&value))
    };

    match result {
        Ok(v) => {
            state.return_data = Vec::from(v).into();
            stack.push(U256::from(0));
        }
        Err(ae) => {
            stack.push(U256::from(ae.exit_code().value()));
        }
    }
    Ok(())
}

pub fn methodnum(state: &mut ExecutionState) {
    state.stack.push(U256::from(state.method));
}
