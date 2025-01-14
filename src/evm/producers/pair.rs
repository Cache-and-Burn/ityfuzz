use crate::evm::input::{ConciseEVMInput, EVMInput};
use crate::evm::types::{EVMAddress, EVMFuzzState, EVMU256};
use crate::evm::vm::EVMState;
use crate::oracle::{OracleCtx, Producer};
use crate::state::HasExecutionResult;
use bytes::Bytes;
use revm_primitives::Bytecode;
use std::collections::HashMap;

pub struct PairProducer {
    pub reserves: HashMap<EVMAddress, (EVMU256, EVMU256)>,
    pub fetch_reserve: Bytes,
}

impl PairProducer {
    pub fn new() -> Self {
        Self {
            reserves: HashMap::new(),
            fetch_reserve: Bytes::from(vec![0x09, 0x02, 0xf1, 0xac]),
        }
    }
}

impl Producer<EVMState, EVMAddress, Bytecode, Bytes, EVMAddress, EVMU256, Vec<u8>, EVMInput, EVMFuzzState, ConciseEVMInput>
    for PairProducer
{
    fn produce(
        &mut self,
        ctx: &mut OracleCtx<
            EVMState,
            EVMAddress,
            Bytecode,
            Bytes,
            EVMAddress,
            EVMU256,
            Vec<u8>,
            EVMInput,
            EVMFuzzState,
            ConciseEVMInput
        >,
    ) {
        #[cfg(feature = "flashloan_v2")]
        {
            let reserves = ctx
                .fuzz_state
                .get_execution_result()
                .new_state
                .state
                .flashloan_data
                .oracle_recheck_reserve
                .clone();
            let mut query_reserves_batch = reserves.iter().map(
                |pair_address| {
                    (*pair_address, self.fetch_reserve.clone())
                }
            ).collect::<Vec<(EVMAddress, Bytes)>>();

            ctx.call_post_batch(&query_reserves_batch).iter().zip(
                reserves.iter()
            ).for_each(
                |(output, pair_address)| {
                    let reserve0 = EVMU256::try_from_be_slice(&output[0..32]).unwrap();
                    let reserve1 = EVMU256::try_from_be_slice(&output[32..64]).unwrap();
                    self.reserves.insert(*pair_address, (reserve0, reserve1));
                }
            );
        }
    }

    fn notify_end(
        &mut self,
        ctx: &mut OracleCtx<
            EVMState,
            EVMAddress,
            Bytecode,
            Bytes,
            EVMAddress,
            EVMU256,
            Vec<u8>,
            EVMInput,
            EVMFuzzState,
            ConciseEVMInput
        >,
    ) {
        self.reserves.clear();
    }
}
