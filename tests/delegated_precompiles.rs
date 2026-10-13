//! EIP-7702 regressions for the dynamic Monad precompile map.

use alloy_evm::{precompiles::DynPrecompile, Evm, EvmEnv, EvmFactory};
use alloy_monad_evm::MonadEvmFactory;
use alloy_primitives::{Address, Bytes, U256};
use monad_revm::{
    precompiles::MonadPrecompiles, reserve_balance::abi::RESERVE_BALANCE_ADDRESS,
    staking::STAKING_ADDRESS, MonadContext, MonadHardfork,
};
use revm::{
    bytecode::opcode,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::InMemoryDB,
    interpreter::{CallInputs, CallOutcome},
    precompile::{u64_to_address, PrecompileId, PrecompileOutput},
    state::{AccountInfo, Bytecode},
    Inspector,
};

const SENDER: Address = Address::repeat_byte(0x11);
const AUTHORITY: Address = Address::repeat_byte(0x22);
const PROXY: Address = Address::repeat_byte(0x33);
const CALL_OPCODES: [u8; 5] = [
    0,
    opcode::CALL,
    opcode::CALLCODE,
    opcode::DELEGATECALL,
    opcode::STATICCALL,
];
const SPECS: [MonadHardfork; 4] = [
    MonadHardfork::MonadEight,
    MonadHardfork::MonadNine,
    MonadHardfork::MonadTen,
    MonadHardfork::MonadNext,
];

#[derive(Debug, Default)]
struct CallRecorder {
    outcomes: Vec<(u64, CallOutcome)>,
}

impl Inspector<MonadContext<InMemoryDB>> for CallRecorder {
    fn call_end(
        &mut self,
        _context: &mut MonadContext<InMemoryDB>,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        self.outcomes.push((inputs.gas_limit, outcome.clone()));
    }
}

fn forward_call(target: Address, call_opcode: u8) -> Bytecode {
    let mut code = vec![
        opcode::CALLDATASIZE,
        opcode::PUSH0,
        opcode::PUSH0,
        opcode::CALLDATACOPY,
        opcode::PUSH0,
        opcode::PUSH0,
        opcode::CALLDATASIZE,
        opcode::PUSH0,
    ];
    if matches!(call_opcode, opcode::CALL | opcode::CALLCODE) {
        code.push(opcode::PUSH0);
    }
    code.push(opcode::PUSH20);
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[
        opcode::PUSH3,
        0x01,
        0x86,
        0xa0,
        call_opcode,
        // Return the status followed by the full returndata.
        opcode::PUSH0,
        opcode::MSTORE,
        opcode::RETURNDATASIZE,
        opcode::PUSH0,
        opcode::PUSH1,
        32,
        opcode::RETURNDATACOPY,
        opcode::RETURNDATASIZE,
        opcode::PUSH1,
        32,
        opcode::ADD,
        opcode::PUSH0,
        opcode::RETURN,
    ]);
    Bytecode::new_raw(code.into())
}

fn database(target: Address, call_opcode: u8, delegated: bool) -> (InMemoryDB, Address) {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        SENDER,
        AccountInfo {
            balance: U256::from(10).pow(U256::from(27)),
            ..Default::default()
        },
    );
    db.insert_account_info(STAKING_ADDRESS, AccountInfo::default());
    db.insert_account_info(
        AUTHORITY,
        AccountInfo::default().with_code(Bytecode::new_eip7702(target)),
    );
    let target = if delegated { AUTHORITY } else { target };
    let destination = if call_opcode == 0 {
        target
    } else {
        db.insert_account_info(
            PROXY,
            AccountInfo::default().with_code(forward_call(target, call_opcode)),
        );
        PROXY
    };
    (db, destination)
}

fn environment(spec: MonadHardfork) -> EvmEnv<MonadHardfork> {
    EvmEnv {
        cfg_env: CfgEnv::new_with_spec(spec),
        block_env: BlockEnv {
            basefee: 0,
            ..Default::default()
        },
    }
}

fn transaction(destination: Address) -> TxEnv {
    TxEnv::builder()
        .caller(SENDER)
        .to(destination)
        .data(Bytes::from_static(b"hello"))
        .gas_limit(1_000_000)
        .gas_price(0)
        .build_fill()
}

fn expected_output(call_opcode: u8, success: bool, output: &[u8]) -> Bytes {
    let mut bytes = if call_opcode == 0 {
        Vec::new()
    } else {
        U256::from(success as u8).to_be_bytes::<32>().to_vec()
    };
    bytes.extend_from_slice(output);
    bytes.into()
}

#[test]
fn dynamic_ethereum_precompiles_execute_empty_code_when_delegated() {
    for spec in SPECS {
        for target in MonadPrecompiles::new_with_spec(spec)
            .precompiles()
            .addresses()
        {
            for call_opcode in CALL_OPCODES {
                for inspected in [false, true] {
                    let (db, destination) = database(*target, call_opcode, true);
                    let mut evm = MonadEvmFactory::default().create_evm_with_inspector(
                        db,
                        environment(spec),
                        CallRecorder::default(),
                    );
                    evm.set_inspector_enabled(inspected);
                    let result = evm.transact(transaction(destination)).unwrap().result;
                    assert!(
                        result.is_success(),
                        "{spec:?}, {target}, opcode {call_opcode}"
                    );
                    assert_eq!(
                        result.output().unwrap(),
                        &expected_output(call_opcode, true, &[])
                    );
                    if inspected {
                        let (gas_limit, outcome) = &evm.components().1.outcomes[0];
                        assert_eq!(outcome.result.gas.remaining(), *gas_limit);
                        assert_eq!(outcome.result.gas.total_gas_spent(), 0);
                    }
                }
            }
        }
    }
}

#[test]
fn direct_dynamic_identity_calls_keep_their_output_and_gas() {
    for spec in SPECS {
        for call_opcode in CALL_OPCODES {
            let (db, destination) = database(u64_to_address(4), call_opcode, false);
            let mut evm = MonadEvmFactory::default().create_evm_with_inspector(
                db,
                environment(spec),
                CallRecorder::default(),
            );
            let result = evm.transact(transaction(destination)).unwrap().result;
            assert!(result.is_success());
            assert_eq!(
                result.output().unwrap(),
                &expected_output(call_opcode, true, b"hello")
            );
            assert_eq!(
                evm.components().1.outcomes[0]
                    .1
                    .result
                    .gas
                    .total_gas_spent(),
                18
            );
        }
    }
}

#[test]
fn native_precompile_delegation_keeps_its_restrictions() {
    for spec in SPECS {
        for target in [STAKING_ADDRESS, RESERVE_BALANCE_ADDRESS] {
            for call_opcode in CALL_OPCODES {
                let (db, destination) = database(target, call_opcode, true);
                let mut evm = MonadEvmFactory::default().create_evm_with_inspector(
                    db,
                    environment(spec),
                    CallRecorder::default(),
                );
                let result = evm.transact(transaction(destination)).unwrap().result;
                let active = target == STAKING_ADDRESS || spec != MonadHardfork::MonadEight;
                assert_eq!(result.is_success(), call_opcode != 0 || !active);
                assert_eq!(
                    result.output().unwrap(),
                    &expected_output(call_opcode, !active, &[])
                );
                let (gas_limit, outcome) = &evm.components().1.outcomes[0];
                assert_eq!(
                    outcome.result.gas.remaining(),
                    if active { 0 } else { *gas_limit }
                );
            }
        }
    }
}

#[test]
fn dynamic_overrides_and_removals_respect_protocol_addresses() {
    let identity = u64_to_address(4);
    let custom = Address::repeat_byte(0x44);
    for target in [identity, custom, STAKING_ADDRESS, RESERVE_BALANCE_ADDRESS] {
        for call_opcode in CALL_OPCODES {
            for delegated in [false, true] {
                for removed in [false, true] {
                    let (db, destination) = database(target, call_opcode, delegated);
                    let mut evm = MonadEvmFactory::default().create_evm_with_inspector(
                        db,
                        environment(MonadHardfork::MonadTen),
                        CallRecorder::default(),
                    );
                    evm.precompiles_mut().apply_precompile(&target, |_| {
                        (!removed).then(|| {
                            DynPrecompile::new(PrecompileId::Custom("Override".into()), |input| {
                                Ok(PrecompileOutput::new(
                                    37,
                                    Bytes::from_static(b"custom"),
                                    input.reservoir,
                                ))
                            })
                        })
                    });
                    let result = evm.transact(transaction(destination)).unwrap().result;
                    let executed = !(removed || delegated && target == identity);
                    assert!(result.is_success());
                    assert_eq!(
                        result.output().unwrap(),
                        &expected_output(call_opcode, true, if executed { b"custom" } else { b"" })
                    );
                    assert_eq!(
                        evm.components().1.outcomes[0]
                            .1
                            .result
                            .gas
                            .total_gas_spent(),
                        if executed { 37 } else { 0 }
                    );
                }
            }
        }
    }
}
