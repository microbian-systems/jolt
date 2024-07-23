use common::constants::virtual_register_index;
use tracer::{ELFInstruction, RVTraceRow, RegisterState, RV32IM};

use super::VirtualInstructionSequence;
use crate::jolt::instruction::{
    add::ADDInstruction, beq::BEQInstruction, mul::MULInstruction,
    virtual_advice::ADVICEInstruction,
    virtual_assert_valid_signed_remainder::AssertValidSignedRemainderInstruction, JoltInstruction,
};

/// Perform signed division and return the remainder
pub struct REMInstruction<const WORD_SIZE: usize>;

impl<const WORD_SIZE: usize> VirtualInstructionSequence for REMInstruction<WORD_SIZE> {
    fn virtual_sequence(instruction: ELFInstruction) -> Vec<ELFInstruction> {
        assert_eq!(instruction.opcode, RV32IM::REM);
        // REM source registers
        let r_x = instruction.rs1;
        let r_y = instruction.rs2;
        // Virtual registers used in sequence
        let v_0 = Some(virtual_register_index(0));
        let v_q = Some(virtual_register_index(1));
        let v_qy = Some(virtual_register_index(2));

        let mut virtual_sequence = vec![];
        virtual_sequence.push(ELFInstruction {
            address: instruction.address,
            opcode: RV32IM::VIRTUAL_ADVICE,
            rs1: None,
            rs2: None,
            rd: v_q,
            imm: None,
            virtual_sequence_index: Some(virtual_sequence.len()),
        });
        virtual_sequence.push(ELFInstruction {
            address: instruction.address,
            opcode: RV32IM::VIRTUAL_ADVICE,
            rs1: None,
            rs2: None,
            rd: instruction.rd,
            imm: None,
            virtual_sequence_index: Some(virtual_sequence.len()),
        });
        virtual_sequence.push(ELFInstruction {
            address: instruction.address,
            opcode: RV32IM::VIRTUAL_ASSERT_VALID_SIGNED_REMAINDER,
            rs1: instruction.rd,
            rs2: r_y,
            rd: None,
            imm: None,
            virtual_sequence_index: Some(virtual_sequence.len()),
        });
        virtual_sequence.push(ELFInstruction {
            address: instruction.address,
            opcode: RV32IM::MUL,
            rs1: v_q,
            rs2: r_y,
            rd: v_qy,
            imm: None,
            virtual_sequence_index: Some(virtual_sequence.len()),
        });
        virtual_sequence.push(ELFInstruction {
            address: instruction.address,
            opcode: RV32IM::ADD,
            rs1: v_qy,
            rs2: instruction.rd,
            rd: v_0,
            imm: None,
            virtual_sequence_index: Some(virtual_sequence.len()),
        });
        virtual_sequence.push(ELFInstruction {
            address: instruction.address,
            opcode: RV32IM::VIRTUAL_ASSERT_EQ,
            rs1: v_0,
            rs2: r_x,
            rd: None,
            imm: None,
            virtual_sequence_index: Some(virtual_sequence.len()),
        });

        virtual_sequence
    }

    fn virtual_trace(trace_row: RVTraceRow) -> Vec<RVTraceRow> {
        assert_eq!(trace_row.instruction.opcode, RV32IM::REM);
        // REM operands
        let x = trace_row.register_state.rs1_val.unwrap();
        let y = trace_row.register_state.rs2_val.unwrap();

        let virtual_instructions = Self::virtual_sequence(trace_row.instruction);
        let mut virtual_trace = vec![];

        let (quotient, remainder) = match WORD_SIZE {
            32 => {
                let mut quotient = x as i32 / y as i32;
                let mut remainder = x as i32 % y as i32;
                if (remainder < 0 && (y as i32) > 0) || (remainder > 0 && (y as i32) < 0) {
                    remainder += y as i32;
                    quotient -= 1;
                }
                (quotient as u32 as u64, remainder as u32 as u64)
            }
            64 => {
                let mut quotient = x as i64 / y as i64;
                let mut remainder = x as i64 % y as i64;
                if (remainder < 0 && (y as i64) > 0) || (remainder > 0 && (y as i64) < 0) {
                    remainder += y as i64;
                    quotient -= 1;
                }
                (quotient as u64, remainder as u64)
            }
            _ => panic!("Unsupported WORD_SIZE: {}", WORD_SIZE),
        };

        let q = ADVICEInstruction::<WORD_SIZE>(quotient).lookup_entry();
        virtual_trace.push(RVTraceRow {
            instruction: virtual_instructions[virtual_trace.len()].clone(),
            register_state: RegisterState {
                rs1_val: None,
                rs2_val: None,
                rd_post_val: Some(q),
            },
            memory_state: None,
            advice_value: Some(quotient),
        });

        let r = ADVICEInstruction::<WORD_SIZE>(remainder).lookup_entry();
        virtual_trace.push(RVTraceRow {
            instruction: virtual_instructions[virtual_trace.len()].clone(),
            register_state: RegisterState {
                rs1_val: None,
                rs2_val: None,
                rd_post_val: Some(r),
            },
            memory_state: None,
            advice_value: Some(remainder),
        });

        let is_valid: u64 = AssertValidSignedRemainderInstruction::<WORD_SIZE>(r, y).lookup_entry();
        assert_eq!(is_valid, 1);
        virtual_trace.push(RVTraceRow {
            instruction: virtual_instructions[virtual_trace.len()].clone(),
            register_state: RegisterState {
                rs1_val: Some(r),
                rs2_val: Some(y),
                rd_post_val: None,
            },
            memory_state: None,
            advice_value: None,
        });

        let q_y = MULInstruction::<WORD_SIZE>(q, y).lookup_entry();
        virtual_trace.push(RVTraceRow {
            instruction: virtual_instructions[virtual_trace.len()].clone(),
            register_state: RegisterState {
                rs1_val: Some(q),
                rs2_val: Some(y),
                rd_post_val: Some(q_y),
            },
            memory_state: None,
            advice_value: None,
        });

        let add_0: u64 = ADDInstruction::<WORD_SIZE>(q_y, r).lookup_entry();
        virtual_trace.push(RVTraceRow {
            instruction: virtual_instructions[virtual_trace.len()].clone(),
            register_state: RegisterState {
                rs1_val: Some(q_y),
                rs2_val: Some(r),
                rd_post_val: Some(add_0),
            },
            memory_state: None,
            advice_value: None,
        });

        let _assert_eq = BEQInstruction(add_0, x).lookup_entry();
        virtual_trace.push(RVTraceRow {
            instruction: virtual_instructions[virtual_trace.len()].clone(),
            register_state: RegisterState {
                rs1_val: Some(add_0),
                rs2_val: Some(x),
                rd_post_val: None,
            },
            memory_state: None,
            advice_value: None,
        });

        virtual_trace
    }
}

#[cfg(test)]
mod test {
    use ark_std::test_rng;
    use common::constants::REGISTER_COUNT;
    use rand_chacha::rand_core::RngCore;

    use crate::jolt::vm::rv32i_vm::RV32I;

    use super::*;

    #[test]
    // TODO(moodlezoup): Turn this into a macro, similar to the `jolt_instruction_test` macro
    fn rem_virtual_sequence_32() {
        let mut rng = test_rng();

        let r_x = rng.next_u64() % 32;
        let r_y = rng.next_u64() % 32;
        let rd = rng.next_u64() % 32;

        let x = rng.next_u32() as u64;
        let y = if r_x == r_y { x } else { rng.next_u32() as u64 };

        let mut remainder = x as i32 % y as i32;
        if (remainder < 0 && (y as i32) > 0) || (remainder > 0 && (y as i32) < 0) {
            remainder += y as i32;
        }
        let result = remainder as u32 as u64;

        let rem_trace_row = RVTraceRow {
            instruction: ELFInstruction {
                address: rng.next_u64(),
                opcode: RV32IM::REM,
                rs1: Some(r_x),
                rs2: Some(r_y),
                rd: Some(rd),
                imm: None,
                virtual_sequence_index: None,
            },
            register_state: RegisterState {
                rs1_val: Some(x),
                rs2_val: Some(y),
                rd_post_val: Some(result as u64),
            },
            memory_state: None,
            advice_value: None,
        };

        let virtual_sequence = REMInstruction::<32>::virtual_trace(rem_trace_row);
        let mut registers = vec![0u64; REGISTER_COUNT as usize];
        registers[r_x as usize] = x;
        registers[r_y as usize] = y;

        for row in virtual_sequence {
            if let Some(rs1_val) = row.register_state.rs1_val {
                assert_eq!(registers[row.instruction.rs1.unwrap() as usize], rs1_val);
            }
            if let Some(rs2_val) = row.register_state.rs2_val {
                assert_eq!(registers[row.instruction.rs2.unwrap() as usize], rs2_val);
            }

            let lookup = RV32I::try_from(&row).unwrap();
            let output = lookup.lookup_entry();
            if let Some(rd) = row.instruction.rd {
                registers[rd as usize] = output;
                assert_eq!(
                    registers[rd as usize],
                    row.register_state.rd_post_val.unwrap()
                );
            } else {
                // Virtual assert instruction
                assert!(output == 1);
            }
        }

        for (index, val) in registers.iter().enumerate() {
            if index as u64 == r_x {
                // Check that r_x hasn't been clobbered
                assert_eq!(*val, x);
            } else if index as u64 == r_y {
                // Check that r_y hasn't been clobbered
                assert_eq!(*val, y);
            } else if index as u64 == rd {
                // Check that result was written to rd
                assert_eq!(*val, result as u64);
            } else if index < 32 {
                // None of the other "real" registers were touched
                assert_eq!(*val, 0);
            }
        }
    }
}
