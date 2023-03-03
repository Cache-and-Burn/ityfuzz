use crate::generic_vm::vm_executor::{ExecutionResult, GenericVM, MAP_SIZE};
use crate::generic_vm::vm_state::VMStateT;
use crate::input::VMInputT;
use crate::r#move::input::{MoveFunctionInput, MoveFunctionInputT};
use crate::r#move::vm_state::MoveVMState;
use crate::state_input::StagedVMState;
use move_binary_format::CompiledModule;
use move_core_types::language_storage::ModuleId;
use move_core_types::resolver::ModuleResolver;
use move_vm_runtime::move_vm;
use move_vm_types::gas::UnmeteredGasMeter;
use move_vm_types::values;
use std::collections::HashMap;
use move_binary_format::access::ModuleAccess;
use move_core_types::account_address::AccountAddress;

struct MoveVM {
    state: MoveVMState,
}

impl<I, S> GenericVM<MoveVMState, CompiledModule, MoveFunctionInput, AccountAddress, values::Value, I, S>
    for MoveVM
where
    I: VMInputT<MoveVMState, AccountAddress> + MoveFunctionInputT,
{
    fn deploy(
        &mut self,
        code: CompiledModule,
        constructor_args: MoveFunctionInput,
        deployed_address: AccountAddress,
    ) -> Option<AccountAddress> {
        // todo(@shou): directly use CompiledModule
        let mut data = vec![];
        code.serialize(&mut data).unwrap();
        let account_modules = self.state.modules.get_mut(&deployed_address);
        match account_modules {
            Some(account_modules) => {
                account_modules.insert(code.name().to_owned(), data);
            }
            None => {
                let mut account_modules = HashMap::new();
                account_modules.insert(code.name().to_owned(), data);
                self.state
                    .modules
                    .insert(deployed_address, account_modules);
            }
        }
        Some(deployed_address)
    }

    fn execute(&mut self, input: &I, state: Option<&mut S>) -> ExecutionResult<MoveVMState>
    where
        MoveVMState: VMStateT,
    {
        let vm = move_vm::MoveVM::new(vec![]).unwrap();
        let mut sess = vm.new_session(&self.state);

        let ret = sess.execute_function_bypass_visibility(
            &input.module_id(),
            &input.function_name(),
            input.ty_args().clone(),
            input.args().clone(),
            &mut UnmeteredGasMeter,
        );

        match ret {
            Ok(ret) => ExecutionResult {
                new_state: StagedVMState::new_with_state(self.state.clone()),
                output: ret
                    .return_values
                    .into_iter()
                    .map(|(bytes, _layout)| bytes)
                    .collect::<Vec<Vec<u8>>>()
                    .into_iter()
                    .flatten()
                    .collect(),
                reverted: false,
            },
            Err(err) => ExecutionResult {
                new_state: StagedVMState::new_uninitialized(),
                output: vec![],
                reverted: false,
            },
        }
    }

    fn get_jmp(&self) -> &'static mut [u8; MAP_SIZE] {
        todo!()
    }

    fn get_read(&self) -> &'static mut [bool; MAP_SIZE] {
        todo!()
    }

    fn get_write(&self) -> &'static mut [u8; MAP_SIZE] {
        todo!()
    }

    fn get_cmp(&self) -> &'static mut [values::Value; MAP_SIZE] {
        todo!()
    }

    fn state_changed(&self) -> bool {
        todo!()
    }
}