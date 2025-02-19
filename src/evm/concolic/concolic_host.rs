use bytes::Bytes;

use crate::evm::abi::BoxedABI;
use crate::evm::input::{ConciseEVMInput, EVMInput, EVMInputT, EVMInputTy};
use crate::evm::middlewares::middleware::MiddlewareType::Concolic;
use crate::evm::middlewares::middleware::{add_corpus, Middleware, MiddlewareType};

use crate::evm::host::{FuzzHost, JMP_MAP};
use crate::generic_vm::vm_executor::MAP_SIZE;
use crate::generic_vm::vm_state::VMStateT;
use crate::input::VMInputT;
use crate::state::{HasCaller, HasCurrentInputIdx, HasItyState};
use either::Either;
use libafl::prelude::{Corpus, HasMetadata, Input};

use libafl::state::{HasCorpus, State};

use revm_interpreter::{Interpreter, Host};
use revm_primitives::{Bytecode, HashMap};

use serde::{Deserialize, Serialize};
use std::borrow::Borrow;

use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::ops::{Add, Mul, Not, Sub};
use std::sync::Arc;
use std::time::{Duration, Instant};
use itertools::Itertools;

use z3::ast::{Bool, BV};
use z3::{ast::Ast, Config, Context, Params, Solver};
use crate::bv_from_u256;
use crate::evm::concolic::concolic_stage::ConcolicPrioritizationMetadata;
use crate::evm::concolic::expr::{ConcolicOp, Expr, simplify, simplify_concat_select};
use crate::evm::types::{as_u64, EVMAddress, EVMU256, is_zero};

pub static mut CONCOLIC_MAP: [u8; MAP_SIZE] = [0; MAP_SIZE];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Field {
    Caller, CallDataValue
}

pub struct Solving<'a> {
    context: &'a Context,
    input: Vec<BV<'a>>,
    balance: &'a BV<'a>,
    calldatavalue: &'a BV<'a>,
    caller: &'a BV<'a>,
    constraints: &'a Vec<Box<Expr>>,
    constrained_field: Vec<Field>
}

impl<'a> Solving<'a> {
    fn new(
        context: &'a Context,
        input: &'a Vec<Box<Expr>>,
        balance: &'a BV<'a>,
        calldatavalue: &'a BV<'a>,
        caller: &'a BV<'a>,
        constraints: &'a Vec<Box<Expr>>,
    ) -> Self {
        Solving {
            context,
            input: (*input).iter().enumerate().map(
                |(idx, x)| {
                    let bv = match &x.op {
                        ConcolicOp::SYMBYTE(name) => {
                            BV::new_const(context, name.clone(), 8)
                        }
                        ConcolicOp::CONSTBYTE(val) => {
                            BV::from_u64(context, *val as u64, 8)
                        }
                        _ => unreachable!("input should be symbolic or concrete"),
                    };
                    bv
                }
            ).collect_vec(),
            balance,
            calldatavalue,
            caller,
            constraints,
            constrained_field: vec![],
        }
    }
}

#[derive(Debug)]
pub enum SymbolicTy<'a> {
    BV(BV<'a>),
    Bool(Bool<'a>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Solution {
    pub input: Vec<u8>,
    pub caller: EVMAddress,
    pub value: EVMU256,
    pub fields: Vec<Field>,
}

impl Solution {
    pub fn to_string(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("(input: {:?}, ", hex::encode(&self.input)));
        s.push_str(&format!("caller: {:?}, ", self.caller));
        s.push_str(&format!("value: {})", self.value));
        s
    }
}

impl<'a> SymbolicTy<'a> {
    pub fn expect_bv(self) -> BV<'a> {
        match self {
            SymbolicTy::BV(bv) => bv,
            _ => panic!("expected bv"),
        }
    }

    pub fn expect_bool(self) -> Bool<'a> {
        match self {
            SymbolicTy::Bool(b) => b,
            _ => panic!("expected bool"),
        }
    }
}

impl<'a> Solving<'a> {
    pub fn slice_input(&self, start: u32, end: u32) -> BV<'a> {
        let start = start as usize;
        let end = end as usize;
        let mut slice = self.input[start].clone();
        for i in start + 1..end {
            if i >= self.input.len() {
                slice = slice.concat(&BV::from_u64(self.context, 0, 8));
            } else {
                slice = slice.concat(&self.input[i]);
            }

        }
        slice
    }

    pub fn generate_z3_bv(&mut self, bv: &Expr, ctx: &'a Context) -> Option<SymbolicTy<'a>> {
        macro_rules! binop {
            ($lhs:expr, $rhs:expr, $op:ident) => {
                {
                    let l = self.generate_z3_bv($lhs.as_ref().unwrap(), ctx);
                    let r = self.generate_z3_bv($rhs.as_ref().unwrap(), ctx);

                    match (l, r) {
                        (Some(SymbolicTy::BV(l)), Some(SymbolicTy::BV(r))) => Some(SymbolicTy::BV(l.$op(&r))),
                        _ => None
                    }
                }

            };
        }
        // println!("generate_z3_bv: {:?}", bv);

        macro_rules! comparisons2 {
            ($lhs:expr, $rhs:expr, $op:ident) => {
                {
                    let lhs = self.generate_z3_bv($lhs.as_ref().unwrap(), ctx);
                    let rhs = self.generate_z3_bv($rhs.as_ref().unwrap(), ctx);
                    match (lhs, rhs) {
                        (Some(SymbolicTy::BV(lhs)), Some(SymbolicTy::BV(rhs))) => Some(SymbolicTy::Bool(lhs.$op(&rhs))),
                        (Some(SymbolicTy::Bool(lhs)), Some(SymbolicTy::Bool(rhs))) => Some(SymbolicTy::Bool(lhs.$op(&rhs))),
                        _ => None
                    }
                }
            };
        }

        macro_rules! comparisons1 {
            ($lhs:expr, $rhs:expr, $op:ident) => {
                {
                    let lhs = self.generate_z3_bv($lhs.as_ref().unwrap(), ctx);
                    let rhs = self.generate_z3_bv($rhs.as_ref().unwrap(), ctx);
                    match (lhs, rhs) {
                        (Some(SymbolicTy::BV(lhs)), Some(SymbolicTy::BV(rhs))) => Some(SymbolicTy::Bool(lhs.$op(&rhs))),
                        _ => None,
                    }
                }
            };
        }

        match &bv.op {
            ConcolicOp::EVMU256(constant) => Some(SymbolicTy::BV(bv_from_u256!(constant, ctx))),
            ConcolicOp::ADD => binop!(bv.lhs, bv.rhs, bvadd),
            ConcolicOp::DIV => binop!(bv.lhs, bv.rhs, bvudiv),
            ConcolicOp::MUL => binop!(bv.lhs, bv.rhs, bvmul),
            ConcolicOp::SUB => binop!(bv.lhs, bv.rhs, bvsub),
            ConcolicOp::SDIV => binop!(bv.lhs, bv.rhs, bvsdiv),
            ConcolicOp::SMOD => binop!(bv.lhs, bv.rhs, bvsmod),
            ConcolicOp::UREM => binop!(bv.lhs, bv.rhs, bvurem),
            ConcolicOp::SREM => binop!(bv.lhs, bv.rhs, bvsrem),
            ConcolicOp::AND => binop!(bv.lhs, bv.rhs, bvand),
            ConcolicOp::OR => binop!(bv.lhs, bv.rhs, bvor),
            ConcolicOp::XOR => binop!(bv.lhs, bv.rhs, bvxor),
            ConcolicOp::NOT => {
                let lhs = self.generate_z3_bv(bv.lhs.as_ref().unwrap(), ctx);
                match lhs {
                    Some(SymbolicTy::BV(lhs)) => Some(SymbolicTy::BV(lhs.bvnot())),
                    Some(SymbolicTy::Bool(lhs)) => Some(SymbolicTy::Bool(lhs.not())),
                    _ => None,
                }
            }
            ConcolicOp::SHL => binop!(bv.lhs, bv.rhs, bvshl),
            ConcolicOp::SHR => binop!(bv.lhs, bv.rhs, bvlshr),
            ConcolicOp::SAR => binop!(bv.lhs, bv.rhs, bvashr),
            ConcolicOp::SLICEDINPUT(idx) => {
                let idx = idx.as_limbs()[0] as u32;
                let skv = self.slice_input(idx, idx + 32);
                // println!("[concolic] SLICEDINPUT: {} {:?}", idx, skv);
                Some(SymbolicTy::BV(skv))
            }
            ConcolicOp::BALANCE => Some(SymbolicTy::BV(self.balance.clone())),
            ConcolicOp::CALLVALUE => {
                self.constrained_field.push(Field::CallDataValue);
                Some(SymbolicTy::BV(self.calldatavalue.clone()))
            },
            ConcolicOp::CALLER => {
                self.constrained_field.push(Field::Caller);
                Some(SymbolicTy::BV(self.caller.clone()))
            },
            ConcolicOp::FINEGRAINEDINPUT(start, end) => {
                                              Some(SymbolicTy::BV(self.slice_input(*start, *end)))
            }
            ConcolicOp::LNOT => {
                let lhs = self.generate_z3_bv(bv.lhs.as_ref().unwrap(), ctx);
                match lhs {
                    Some(SymbolicTy::BV(lhs)) => Some(SymbolicTy::BV(lhs.not())),
                    Some(SymbolicTy::Bool(lhs)) => Some(SymbolicTy::Bool(lhs.not())),
                    _ => None,
                }
            }
            ConcolicOp::CONSTBYTE(b) => Some(SymbolicTy::BV(BV::from_u64(ctx, *b as u64, 8))),
            ConcolicOp::SYMBYTE(s) => Some(SymbolicTy::BV(BV::new_const(ctx, s.clone(), 8))),
            ConcolicOp::EQ => comparisons2!(bv.lhs, bv.rhs, _eq),
            ConcolicOp::LT => comparisons1!(bv.lhs, bv.rhs, bvult),
            ConcolicOp::SLT => comparisons1!(bv.lhs, bv.rhs, bvslt),
            ConcolicOp::GT => comparisons1!(bv.lhs, bv.rhs, bvugt),
            ConcolicOp::SGT => comparisons1!(bv.lhs, bv.rhs, bvsgt),
            ConcolicOp::SELECT(high, low) => {
                let lhs = self.generate_z3_bv(bv.lhs.as_ref().unwrap(), ctx);
                match lhs {
                    Some(SymbolicTy::BV(lhs)) => {
                        Some(SymbolicTy::BV(lhs.extract(*high, *low)))
                    },
                    _ => None,
                }
            },

            ConcolicOp::CONCAT => {
                let lhs = self.generate_z3_bv(bv.lhs.as_ref().unwrap(), ctx);
                let rhs = self.generate_z3_bv(bv.rhs.as_ref().unwrap(), ctx);
                match (lhs, rhs) {
                    (Some(SymbolicTy::BV(lhs)), Some(SymbolicTy::BV(rhs))) => {
                        Some(SymbolicTy::BV(lhs.concat(&rhs)))
                    },
                    _ => None,
                }
            }
        }
    }

    pub fn solve(&mut self) -> Vec<Solution> {
        let context = self.context;
        let solver = Solver::new(&context);
        // println!("Constraints: {:?}", self.constraints);
        for cons in self.constraints {
            let bv = self.generate_z3_bv(&cons.lhs.as_ref().unwrap(), &context);

            macro_rules! expect_bv_or_continue {
                ($e: expr) => {
                    if let Some(SymbolicTy::BV(bv)) = $e {
                        bv
                    } else {
                        #[cfg(feature = "z3_debug")]
                        println!("Failed to generate Z3 BV for {:?}", $e);
                        continue;
                    }
                };
            }

            solver.assert(&match cons.op {
                ConcolicOp::GT => expect_bv_or_continue!(bv).bvugt(
                    &expect_bv_or_continue!(self
                        .generate_z3_bv(&cons.rhs.as_ref().unwrap(), &context)),
                ),
                ConcolicOp::SGT => expect_bv_or_continue!(bv).bvsgt(
                    &expect_bv_or_continue!(self
                        .generate_z3_bv(&cons.rhs.as_ref().unwrap(), &context)),
                ),
                ConcolicOp::EQ => expect_bv_or_continue!(bv)._eq(
                    &expect_bv_or_continue!(self
                        .generate_z3_bv(&cons.rhs.as_ref().unwrap(), &context)),
                ),
                ConcolicOp::LT => expect_bv_or_continue!(bv).bvult(
                    &expect_bv_or_continue!(self
                        .generate_z3_bv(&cons.rhs.as_ref().unwrap(), &context)),
                ),
                ConcolicOp::SLT => expect_bv_or_continue!(bv).bvslt(
                    &expect_bv_or_continue!(self
                        .generate_z3_bv(&cons.rhs.as_ref().unwrap(), &context)),
                ),
                ConcolicOp::LNOT => match bv {
                    Some(SymbolicTy::BV(bv)) => bv._eq(&bv_from_u256!(EVMU256::ZERO, &context)),
                    Some(SymbolicTy::Bool(bv)) => bv.not(),
                    _ => {
                        #[cfg(feature = "z3_debug")]
                        println!("Failed to generate Z3 BV for {:?}", bv);
                        continue;
                    },
                },
                _ => {
                    #[cfg(feature = "z3_debug")]
                    println!("Unsupported constraint: {:?}", cons);
                    continue;
                }
            });
        }

        // println!("Solver: {:?}", solver);
        let mut p = Params::new(&context);
        p.set_u32("timeout", 1000);
        solver.set_params(&p);
        let result = solver.check();
        match result {
            z3::SatResult::Sat => {
                let model = solver.get_model().unwrap();
                #[cfg(feature = "z3_debug")]
                println!("Model: {:?}", model);
                let input = self.input
                    .iter()
                    .map(|x| format!("{}", model.eval(x, true).unwrap())
                        .trim_start_matches("#x")
                        .to_string())
                    .collect::<Vec<_>>()
                    .join("");
                let input_bytes = hex::decode(input.clone()).unwrap();
                let callvalue = model.eval(self.calldatavalue, true).unwrap().to_string();
                let callvalue_int = EVMU256::from_str_radix(&callvalue.trim_start_matches("#x"), 16).unwrap();
                let caller = model.eval(self.caller, true).unwrap().to_string();
                let caller_addr = EVMAddress::from_slice(&hex::decode(caller.as_str()[26..66].to_string()).unwrap());
                vec![Solution {
                    input: input_bytes,
                    caller: caller_addr,
                    value: callvalue_int,
                    fields: self.constrained_field.clone(),
                }]
            }
            z3::SatResult::Unsat => vec![],
            z3::SatResult::Unknown => vec![],
        }
    }
}

// Note: To model concolic memory, we need to remember previous constraints as well.
// when solving a constraint involving persistant memory, if the persistant memory is not
// depenent on other non-persitent variables, this means that the persistant memory change
// might not be feasible, because the persistant memory cannot change it self.
// Example:
//     // in this case, even if we get the constraints for the memory element m[0]
//     // we cannot solve it (invert it), because the memory element is cannot change
//     // it self.
//     m = [0, 0, 0, 0]
//     fn f(a):
//         if m[0] == 0:
//             do something
//         else:
//             bug
//     // in this case, we can actually solve for m[0]!=0, becuase the memeory element
//     // is dependent on the input a.
//     fn g(a):
//         m[0] = a
//         if m[0] == 0:
//             do something
//         else:
//             bug

// #[derive(Debug, Clone, Serialize, Deserialize)]
// pub struct EVMInputConstraint {
//     // concrete data of EVM Input
//     data: Bytes,
//     input_constraints: Vec<Box<Expr>>,
// }

// impl EVMInputConstraint {
//     pub fn new(vm_input: BoxedABI) -> Self {
//         // TODO: build input constraints from ABI
//         let mut input_constraints = vec![];
//         // input_constraints.push()

//         Self {
//             data: Bytes::from(vm_input.get_bytes()),
//             input_constraints: input_constraints,
//         }
//     }

//     pub fn add_constraint(&mut self, constraint: Box<Expr>) {
//         self.input_constraints.push(constraint);
//     }

//     pub fn get_constraints(&self) -> &Vec<Box<Expr>> {
//         &self.input_constraints
//     }

//     pub fn get_data(&self) -> &Bytes {
//         &self.data
//     }
// }

// Q: Why do we need to make persistent memory symbolic?

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolicMemory {
    /// Memory is a vector of bytes, each byte is a symbolic value
    pub memory: Vec<Option<Box<Expr>>>,
    // pub memory_32: Vec<Option<Box<Expr>>>,
}

impl SymbolicMemory {
    pub fn new() -> Self {
        Self {
            memory: vec![],
            // memory_32: vec![],
        }
    }

    pub fn insert_256(&mut self, idx: EVMU256, val: Box<Expr>) {
        let idx = idx.as_limbs()[0] as usize;
        if idx + 32 >= self.memory.len() {
            self.memory.resize(idx + 32 + 1, None);
            // self.memory_32.resize(idx / 32 + 1, None);
        }

        // if idx % 32 == 0 {
        //     self.memory_32[idx / 32] = Some(val.clone());
        // }

        for i in 0..32 {
            let i_u32 = i as u32;
            self.memory[idx + i] = Some(Box::new(Expr {
                lhs: Some(val.clone()),
                rhs: None,
                op: ConcolicOp::SELECT(256 - i_u32*8 - 1, 256 - i_u32*8 - 7 - 1),
            }));
        }
    }

    pub fn insert_8(&mut self, idx: EVMU256, val: Box<Expr>) {
        // TODO: use SELECT instead of concrete value
        let idx = idx.as_limbs()[0] as usize;
        if idx >= self.memory.len() {
            self.memory.resize(idx + 1, None);
        }

        println!("insert_8: idx: {}, val: {:?}", idx, val);
        todo!("insert_8");
        // self.memory[idx] = Some(Box::new(Expr {
        //     lhs: Some(val.clone()),
        //     rhs: None,
        //     op: ConcolicOp::SELECT(31 - i_u32*8, 24 - i_u32*8),
        // }));
    }

    pub fn get_256(&self, idx: EVMU256) -> Option<Box<Expr>> {
        let idx = idx.as_limbs()[0] as usize;
        if idx >= self.memory.len() {
            return None;
        }

        // if idx % 32 == 0 {
        //     return self.memory_32[idx / 32].clone();
        // }

        let mut all_bytes = if let Some(by) = self.memory[idx].clone() {
            by
        } else {
            Box::new(
                Expr {
                    lhs: None,
                    rhs: None,
                    op: ConcolicOp::CONSTBYTE(0),
                }
            )
        };
        for i in 1..32 {
            all_bytes = Box::new(
                Expr {
                    lhs: Some(all_bytes),
                    rhs: if let Some(by) = self.memory[idx + i].clone() {
                        Some(by)
                    } else {
                        Some(Box::new(Expr {
                            lhs: None,
                            rhs: None,
                            op: ConcolicOp::CONSTBYTE(0),
                        }))
                    },
                    op: ConcolicOp::CONCAT,
                }
            );
        }


        Some(simplify(all_bytes))
    }

    pub fn get_slice(&mut self, idx: EVMU256, len: EVMU256) -> Vec<Box<Expr>> {
        let idx = idx.as_limbs()[0] as usize;
        let len = len.as_limbs()[0] as usize;

        if idx + len >= self.memory.len() {
            self.memory.resize(idx + len + 1, None);
        }

        let mut result = vec![];

        for i in idx..(idx + len) {
            if i >= self.memory.len() {
                result.push(Box::new(Expr {
                    lhs: None,
                    rhs: None,
                    op: ConcolicOp::CONSTBYTE(0),
                }));
            } else {
                result.push(if let Some(by) = self.memory[i].clone() {
                    by
                } else {
                    Box::new(Expr {
                        lhs: None,
                        rhs: None,
                        op: ConcolicOp::CONSTBYTE(0),
                    })
                });
            }
        }
        result
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConcolicCallCtx {
    pub symbolic_stack: Vec<Option<Box<Expr>>>,
    pub symbolic_memory: SymbolicMemory,
    pub symbolic_state: HashMap<EVMU256, Option<Box<Expr>>>,

    // seperated by 32 bytes
    pub input_bytes: Vec<Box<Expr>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ConcolicHost<I, VS> {
    pub symbolic_stack: Vec<Option<Box<Expr>>>,
    pub symbolic_memory: SymbolicMemory,
    pub symbolic_state: HashMap<EVMU256, Option<Box<Expr>>>,
    pub input_bytes: Vec<Box<Expr>>,
    pub constraints: Vec<Box<Expr>>,
    pub testcase_ref: Arc<EVMInput>,

    pub ctxs: Vec<ConcolicCallCtx>,
    pub phantom: PhantomData<(I, VS)>,
}

impl<I, VS> ConcolicHost<I, VS> {
    pub fn new(testcase_ref: Arc<EVMInput>) -> Self {
        Self {
            symbolic_stack: Vec::new(),
            symbolic_memory: SymbolicMemory::new(),
            symbolic_state: Default::default(),
            input_bytes: Self::construct_input_from_abi(testcase_ref.get_data_abi().expect("data abi not found")),
            constraints: vec![],
            testcase_ref,
            phantom: Default::default(),
            ctxs: vec![],
        }
    }

    pub fn pop_ctx(&mut self) {
        let ctx = self.ctxs.pop();
        if let Some(ctx) = ctx {
            self.symbolic_stack = ctx.symbolic_stack;
            self.symbolic_memory = ctx.symbolic_memory;
            self.symbolic_state = ctx.symbolic_state;
        } else {
            panic!("pop_ctx: ctx is empty");
        }

    }

    pub fn push_ctx(&mut self, interp: &mut Interpreter) {
        // interp.stack.data()[interp.stack.len() - 1 - $idx]
        let (arg_offset, arg_len) = match unsafe { *interp.instruction_pointer } {
            0xf1 | 0xf2 => {
                (
                    interp.stack.peek(3).unwrap(),
                    interp.stack.peek(4).unwrap(),
                )
            }
            0xf4 | 0xfa => {
                (
                    interp.stack.peek(2).unwrap(),
                    interp.stack.peek(3).unwrap(),
                )
            }
            _ => {
                panic!("not supported opcode");
            }
        };

        let ctx = ConcolicCallCtx {
            symbolic_stack: self.symbolic_stack.clone(),
            symbolic_memory: self.symbolic_memory.clone(),
            symbolic_state: self.symbolic_state.clone(),
            input_bytes: {
                let by = self.symbolic_memory.get_slice(arg_offset, arg_len);
                #[cfg(feature = "z3_debug")]
                {
                    println!("input_bytes = {} {}", arg_offset, arg_len);
                    by.iter().for_each(|b| {
                        b.pretty_print();
                    });
                }
                by
            },
        };
        self.ctxs.push(ctx);

        self.symbolic_stack = vec![];
        self.symbolic_memory = SymbolicMemory::new();
        self.symbolic_state = Default::default();
    }

    fn construct_input_from_abi(vm_input: BoxedABI) -> Vec<Box<Expr>> {
        let res = vm_input.get_concolic();
        // println!("[concolic] construct_input_from_abi: {:?}", res);
        res
    }

    fn string_to_bytes(s: &str) -> Vec<u8> {
        // s: #x....
        hex::decode(&s[2..]).unwrap()
    }

    pub fn solve(&self) -> Vec<Solution> {
        let context = Context::new(&Config::default());
        // let input = (0..self.bytes)
        //     .map(|idx| BV::new_const(&context, format!("input_{}", idx), 8))
        //     .collect::<Vec<_>>();
        let callvalue = BV::new_const(&context, "callvalue", 256);
        let caller = BV::new_const(&context, "caller", 256);
        let balance = BV::new_const(&context, "balance", 256);

        let mut solving = Solving::new(&context, &self.input_bytes, &balance, &callvalue, &caller, &self.constraints);
        solving.solve()
    }

    pub fn get_input_slice_from_ctx(&self, idx: usize, length: usize) -> Box<Expr> {
        let mut data = self.ctxs.last().expect("no ctx").input_bytes.clone();
        let mut bytes = data[idx].clone();
        for i in idx + 1..idx + length {
            if i >= data.len() {
                bytes = bytes.concat(Expr::const_byte(0));
            } else {
                bytes = bytes.concat(data[i].clone());
            }
        }
        simplify(bytes)
    }
}

// TODO: test this
fn str_to_bytes(s: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for c in s.chars() {
        bytes.push(c as u8);
    }
    bytes
}

impl<I, VS, S> Middleware<VS, I, S> for ConcolicHost<I, VS>
where
    I: Input + VMInputT<VS, EVMAddress, EVMAddress, ConciseEVMInput> + EVMInputT + 'static,
    VS: VMStateT,
    S: State
        + HasCaller<EVMAddress>
        + HasCorpus<I>
        + HasItyState<EVMAddress, EVMAddress, VS, ConciseEVMInput>
        + HasMetadata
        + HasCurrentInputIdx
        + Debug
        + Clone,
{
    unsafe fn on_step(
        &mut self,
        interp: &mut Interpreter,
        host: &mut FuzzHost<VS, I, S>,
        state: &mut S,
    ) {
        macro_rules! fast_peek {
            ($idx:expr) => {
                    interp.stack.data()[interp.stack.len() - 1 - $idx]
            };
        }

        macro_rules! stack_bv {
            ($idx:expr) => {{
                let real_loc_sym = self.symbolic_stack.len() - 1 - $idx;
                match self.symbolic_stack[real_loc_sym].borrow() {
                    Some(bv) => bv.clone(),
                    None => {
                        let u256 = fast_peek!($idx);
                        Box::new(Expr {
                            lhs: None,
                            rhs: None,
                            op: ConcolicOp::EVMU256(u256),
                        })
                    }
                }
            }};
        }

        macro_rules! stack_concrete {
            ($idx:expr) => {{
                let real_loc_conc = interp.stack.len() - 1 - $idx;
                let u256 = interp.stack.peek(real_loc_conc).expect("stack underflow");
                u256
            }};
        }

        macro_rules! concrete_eval {
            ($in_cnt: expr, $out_cnt: expr) => {
                {
                    // println!("[concolic] concrete_eval: {} {}", $in_cnt, $out_cnt);
                    for _ in 0..$in_cnt {
                        self.symbolic_stack.pop();
                    }
                    vec![None; $out_cnt]
                }
            };
        }

        macro_rules! concrete_eval_with_action {
            ($in_cnt: expr, $out_cnt: expr, $pp: ident) => {
                {
                    // println!("[concolic] concrete_eval: {} {}", $in_cnt, $out_cnt);
                    for _ in 0..$in_cnt {
                        self.symbolic_stack.pop();
                    }
                    for _ in 0..$out_cnt {
                        self.symbolic_stack.push(None);
                    }
                    self.$pp(interp);
                    vec![]
                }
            };
        }


        let mut solutions = vec![];

        // if self.ctxs.len() > 0 {
        //     return;
        // }

        // TODO: Figure out the corresponding MiddlewareOp to add
        // We may need coverage map here to decide whether to add a new input to the
        // corpus or not.
        // println!("[concolic] on_step @ {:x}: {:x}", interp.program_counter(), *interp.instruction_pointer);
        // println!("[concolic] stack: {:?}", interp.stack.len());
        // println!("[concolic] symbolic_stack: {:?}", self.symbolic_stack.len());

        // let mut max_depth = 0;
        // let mut max_ref = None;
        // for s in &self.symbolic_stack {
        //     if let Some(bv) = s {
        //         let depth = bv.depth();
        //         if depth > max_depth {
        //             max_depth = depth;
        //             max_ref = Some(bv);
        //         }
        //     }
        // }
        //
        // println!("max_depth: {} for {:?}", max_depth, max_ref.map(|x| x.pretty_print_str()));
        // println!("max_depth simpl: {:?} for {:?}", max_ref.map(|x| simplify(x.clone()).depth()), max_ref.map(|x| simplify(x.clone()).pretty_print_str()));
        #[cfg(feature = "z3_debug")]
        {
            println!("[concolic] on_step @ {:x}: {:x}", interp.program_counter(), *interp.instruction_pointer);
            println!("[concolic] stack: {:?}", interp.stack);
            println!("[concolic] symbolic_stack: {:?}", self.symbolic_stack);
            for idx in 0..interp.stack.len() {
                let real = interp.stack.data[idx].clone();
                let sym = self.symbolic_stack[idx].clone();
                if sym.is_some() {
                    match sym.unwrap().op {
                        ConcolicOp::EVMU256(v) => {
                            assert_eq!(real, v);
                        }
                        _ => {}
                    }
                }
            }
            assert_eq!(interp.stack.len(), self.symbolic_stack.len());
        }

        let bv: Vec<Option<Box<Expr>>> = match *interp.instruction_pointer {
            // ADD
            0x01 => {
                let res = Some(stack_bv!(0).add(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // MUL
            0x02 => {
                let res = Some(stack_bv!(0).mul(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // SUB
            0x03 => {
                let res = Some(stack_bv!(0).sub(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // DIV - is this signed?
            0x04 => {
                let res = Some(stack_bv!(0).div(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // SDIV
            0x05 => {
                let res = Some(stack_bv!(0).bvsdiv(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // MOD
            0x06 => {
                let res = Some(stack_bv!(0).bvurem(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // SMOD
            0x07 => {
                let res = Some(stack_bv!(0).bvsmod(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // ADDMOD
            0x08 => {
                let res = Some(stack_bv!(0).add(stack_bv!(1)).bvsmod(stack_bv!(2)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // MULMOD
            0x09 => {
                let res = Some(stack_bv!(0).mul(stack_bv!(1)).bvsmod(stack_bv!(2)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // EXP - fallback to concrete due to poor Z3 performance support
            0x0a => {
                concrete_eval!(2, 1)
            }
            // SIGNEXTEND - FIXME: need to check
            0x0b => {
                concrete_eval!(2, 1)
            }
            // LT
            0x10 => {
                let res = Some(stack_bv!(0).bvult(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // GT
            0x11 => {
                let res = Some(stack_bv!(0).bvugt(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // SLT
            0x12 => {
                let res = Some(stack_bv!(0).bvslt(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // SGT
            0x13 => {
                let res = Some(stack_bv!(0).bvsgt(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // EQ
            0x14 => {
                let res = Some(stack_bv!(0).equal(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // ISZERO
            0x15 => {
                let res = Some(stack_bv!(0).equal(Box::new(Expr {
                    lhs: None,
                    rhs: None,
                    op: ConcolicOp::EVMU256(EVMU256::from(0)),
                })));
                self.symbolic_stack.pop();
                vec![res]
            }
            // AND
            0x16 => {
                let res = Some(stack_bv!(0).bvand(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // OR
            0x17 => {
                let res = Some(stack_bv!(0).bvor(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // XOR
            0x18 => {
                let res = Some(stack_bv!(0).bvxor(stack_bv!(1)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // NOT
            0x19 => {
                let res = Some(stack_bv!(0).bvnot());
                self.symbolic_stack.pop();
                vec![res]
            }
            // BYTE
            // FIXME: support this
            0x1a => {
                concrete_eval!(2, 1)
            }
            // SHL
            0x1b => {
                let res = Some(stack_bv!(1).bvshl(stack_bv!(0)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // SHR
            0x1c => {
                let res = Some(stack_bv!(1).bvlshr(stack_bv!(0)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // SAR
            0x1d => {
                let res = Some(stack_bv!(1).bvsar(stack_bv!(0)));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![res]
            }
            // SHA3
            0x20 => {
                concrete_eval!(2, 1)
            }
            // ADDRESS
            0x30 => {
                vec![None]
            }
            // BALANCE
            // TODO: need to get value from a hashmap
            0x31 => {
                concrete_eval!(1, 1)
            }
            // ORIGIN
            0x32 => {
                vec![None]
            }
            // CALLER
            0x33 => {
                // println!("CALLER @ pc : {:x}", interp.program_counter());
                if self.ctxs.len() > 0 {
                    // use concrete caller when inside a call
                    vec![None]
                } else {
                    vec![Some(Expr::new_caller())]
                }
            }
            // CALLVALUE
            0x34 => {

                if self.ctxs.len() > 0 {
                    // use concrete caller when inside a call
                    vec![None]
                } else {
                    vec![Some(Expr::new_callvalue())]
                }
            }
            // CALLDATALOAD
            0x35 => {
                let offset = interp.stack.peek(0).unwrap();
                self.symbolic_stack.pop();
                if self.ctxs.len() > 0 {
                    let offset_usize = as_u64(offset) as usize;
                    #[cfg(feature = "z3_debug")]
                    {
                        println!("CALLDATALOAD: {:?}", self.get_input_slice_from_ctx(offset_usize, 32));
                        self.get_input_slice_from_ctx(offset_usize, 32).pretty_print();
                    }
                    vec![Some(self.get_input_slice_from_ctx(offset_usize, 32))]
                } else {
                    vec![Some(Expr::new_sliced_input(offset))]
                }
            }
            // CALLDATASIZE
            0x36 => {
                vec![None]
            }
            // CALLDATACOPY
            0x37 => {
                concrete_eval!(3, 0)
            }
            // CODESIZE
            0x38 => {
                vec![None]
            }
            // CODECOPY
            0x39 => {
                concrete_eval!(3, 0)
            }
            // GASPRICE
            0x3a => {
                vec![None]
            }
            // EXTCODESIZE
            0x3b => {
                concrete_eval!(1, 1)
            }
            // EXTCODECOPY
            0x3c => {
                concrete_eval!(4, 0)
            }
            // RETURNDATASIZE
            0x3d => {
                vec![None]
            }
            // RETURNDATACOPY
            0x3e => {
                concrete_eval!(3, 0)
            }
            // EXTCODEHASH
            0x3f => {
                concrete_eval!(1, 1)
            }
            // BLOCKHASH
            0x40 => {
                concrete_eval!(1, 1)
            }
            // COINBASE
            0x41 => {
                vec![None]
            }
            // TIMESTAMP
            0x42 => {
                vec![None]
            }
            // NUMBER
            0x43 => {
                vec![None]
            }
            // PREVRANDAO
            0x44 => {
                vec![None]
            }
            // GASLIMIT
            0x45 => {
                vec![None]
            }
            // CHAINID
            0x46 => {
                vec![None]
            }
            // SELFBALANCE
            0x47 => {
                vec![None]
            }
            // BASEFEE
            0x48 => {
                vec![None]
            }
            // POP
            0x50 => {
                self.symbolic_stack.pop();
                vec![]
            }
            // MLOAD
            0x51 => {
                // println!("[concolic] MLOAD: {:?}", self.symbolic_stack);
                let offset = fast_peek!(0);
                self.symbolic_stack.pop();
                vec![self.symbolic_memory.get_256(offset)]
            }
            // MSTORE
            0x52 => {
                let offset = fast_peek!(0);
                let value = stack_bv!(1);
                self.symbolic_memory.insert_256(offset, value);
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![]
            }
            // MSTORE8
            0x53 => {
                let offset = fast_peek!(0);
                let value = stack_bv!(1);
                self.symbolic_memory.insert_8(offset, value);
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![]
            }
            // SLOAD
            0x54 => {
                self.symbolic_stack.pop();
                let key = fast_peek!(0);
                vec![match self.symbolic_state.get(&key) {
                    Some(v) => v.clone(),
                    None => None,
                }]
            }
            // SSTORE
            0x55 => {
                let key = fast_peek!(1);
                let value = stack_bv!(0);
                self.symbolic_state.insert(key, Some(value));
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![]
            }
            // JUMP
            0x56 => {
                concrete_eval!(1, 0)
            }
            // JUMPI
            0x57 => {
                // println!("{:?}", interp.stack);
                // println!("{:?}", self.symbolic_stack);
                // jump dest in concolic solving mode is the opposite of the concrete
                let br = is_zero(fast_peek!(1));
                let intended_jmp_dest = if !br {
                    1
                } else {
                    as_u64(fast_peek!(0))
                };

                let real_path_constraint = if br {
                    // path_condition = false
                    stack_bv!(1).lnot()
                } else {
                    // path_condition = true
                    stack_bv!(1)
                };

                let idx = (interp.program_counter() * (intended_jmp_dest as usize)) % MAP_SIZE;
                if JMP_MAP[idx] == 0 && !real_path_constraint.is_concrete() {
                    let intended_path_constraint = real_path_constraint.clone().lnot();
                    #[cfg(feature = "z3_debug")]
                    println!("[concolic] to solve {:?}", intended_path_constraint.pretty_print_str());
                    self.constraints.push(intended_path_constraint);

                    solutions.extend(self.solve());
                    #[cfg(feature = "z3_debug")]
                    println!("[concolic] Solutions: {:?}", solutions);
                    self.constraints.pop();
                }
                // jumping only happens if the second element is false
                if !real_path_constraint.is_concrete() {
                    self.constraints.push(real_path_constraint);
                }
                self.symbolic_stack.pop();
                self.symbolic_stack.pop();
                vec![]
            }
            // PC
            0x58 => {
                vec![None]
            }
            // MSIZE
            0x59 => {
                vec![None]
            }
            // GAS
            0x5a => {
                vec![None]
            }
            // JUMPDEST
            0x5b => {
                vec![]
            }
            // PUSH
            0x60..=0x7f => {
                // push n bytes into stack
                // Concolic push n bytes is equivalent to concrete push, because the bytes
                // being pushed are always concrete, we can just push None to the stack
                // and 'fallthrough' to concrete values later
                vec![None]
            }
            // DUP
            0x80..=0x8f => {
                let _n = (*interp.instruction_pointer) - 0x80;
                vec![Some(stack_bv!(usize::from(_n)).clone())]
            }
            // SWAP
            0x90..=0x9f => {
                let _n = (*interp.instruction_pointer) - 0x90 + 1;
                let swapper = stack_bv!(usize::from(_n));
                let swappee = stack_bv!(0);
                let symbolic_stack_len = self.symbolic_stack.len();
                self.symbolic_stack[symbolic_stack_len - 1] = Some(swapper);
                self.symbolic_stack[symbolic_stack_len - usize::from(_n) - 1] = Some(swappee);
                vec![]
            }
            // LOG
            0xa0..=0xa4 => {
                let _n = (*interp.instruction_pointer) - 0xa0;
                concrete_eval!(_n + 2, 0)
            }
            // CREATE
            0xf0 => {
                concrete_eval!(3, 1)
            }
            // CALL
            0xf1 => {
                concrete_eval_with_action!(7, 1, push_ctx)
            }
            // CALLCODE
            0xf2 => {
                concrete_eval_with_action!(7, 1, push_ctx)
            }
            // RETURN
            0xf3 => {
                vec![]
            }
            // DELEGATECALL
            0xf4 => {
                concrete_eval_with_action!(6, 1, push_ctx)
            }
            // CREATE2
            0xf5 => {
                concrete_eval!(4, 1)
            }
            // STATICCALL
            0xfa => {
                concrete_eval_with_action!(6, 1, push_ctx)
            }
            // REVERT
            0xfd => {
                concrete_eval!(2, 0)
            }
            // INVALID
            0xfe => {
                vec![]
            }
            // SELFDESTRUCT
            0xff => {
                concrete_eval!(1, 0)
            }
            // STOP
            0x00 => {
                vec![]
            }
            _ => {
                panic!("Unsupported opcode: {:?}", *interp.instruction_pointer);
                vec![]
            }
        };
        // println!("[concolic] adding bv to stack {:?}", bv);
        for v in bv {
            if v.is_some() && v.as_ref().unwrap().is_concrete() {
                self.symbolic_stack.push(None);
            } else {
                self.symbolic_stack.push(v);
            }
        }

        // let input = state
        //     .corpus()
        //     .get(state.get_current_input_idx())
        //     .unwrap()
        //     .borrow_mut()
        //     .load_input()
        //     .expect("Failed loading input")
        //     .clone();

        if solutions.len() > 0 {
            let meta = state.metadata_mut().get_mut::<ConcolicPrioritizationMetadata>().expect("Failed to get metadata");
            for solution in solutions {
                meta.solutions.push((solution, self.testcase_ref.clone()));
            }
        }

    }

    unsafe fn on_return(
        &mut self,
        interp: &mut Interpreter,
        host: &mut FuzzHost<VS, I, S>,
        state: &mut S,
    ) {
        self.pop_ctx();
    }

    unsafe fn on_insert(&mut self, bytecode: &mut Bytecode, address: EVMAddress, host: &mut FuzzHost<VS, I, S>, state: &mut S) {

    }

    fn get_type(&self) -> MiddlewareType {
        Concolic
    }
}
