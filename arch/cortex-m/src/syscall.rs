//! Implementation of the architecture-specific portions of the kernel-userland
//! system call interface.

use core::fmt::Write;
use core::marker::PhantomData;
use core::mem::{self, size_of};
use core::ops::Range;
use core::ptr::{self, read_volatile, write_volatile};
use kernel::errorcode::ErrorCode;

use crate::scb;
use crate::CortexMVariant;
use kernel::debug;

use kernel::platform::chip::Chip;
use kernel::platform::platform::KernelResources;
use kernel::process;

use crate::dwt;
use crate::PROCESS_ID;
// use crate::PROCESS;

/// This is used in the syscall handler. When set to 1 this means the
/// svc_handler was called. Marked `pub` because it is used in the cortex-m*
/// specific handler.
#[no_mangle]
#[used]
pub static mut SYSCALL_FIRED: usize = 0;

/// This is called in the hard fault handler. When set to 1 this means the hard
/// fault handler was called. Marked `pub` because it is used in the cortex-m*
/// specific handler.
///
/// n.b. If the kernel hard faults, it immediately panic's. This flag is only
/// for handling application hard faults.
#[no_mangle]
#[used]
pub static mut APP_HARD_FAULT: usize = 0;

/// This is used in the hardfault handler. When an app faults, the hardfault
/// handler stores the value of the SCB registers in this static array. This
/// makes them available to be displayed in a diagnostic fault message.
#[no_mangle]
#[used]
pub static mut SCB_REGISTERS: [u32; 5] = [0; 5];

/// Stores the current value of the process stack pointer
#[no_mangle]
#[used]
pub static mut PROCESS_STACK_POINTER: *const usize = &0usize as *const usize;

/// Stores the current value of the process registers
#[no_mangle]
#[used]
pub static mut PROCESS_REGS: [usize; 8] = [0; 8];

/// Stores a reference to current Process Stored State
#[no_mangle]
#[used]
pub static mut PROCESS_STATE: *const usize = &0usize as *const usize;

/// Stores the current value of the benchmarking counter
#[no_mangle]
#[used]
pub static mut COUNTER: usize = 0;

// Space for 8 u32s: r0-r3, r12, lr, pc, and xPSR
const SVC_FRAME_SIZE: usize = 32;

/// This holds all of the state that the kernel must keep for the process when
/// the process is not executing.
#[derive(Default)]
pub struct CortexMStoredState {
    regs: [usize; 8],
    yield_pc: usize,
    psr: usize,
    psp: usize,
}

/// Values for encoding the stored state buffer in a binary slice.
const VERSION: usize = 1;
const STORED_STATE_SIZE: usize = size_of::<CortexMStoredState>();
const TAG: [u8; 4] = [b'c', b't', b'x', b'm'];
const METADATA_LEN: usize = 3;

const VERSION_IDX: usize = 0;
const SIZE_IDX: usize = 1;
const TAG_IDX: usize = 2;
const YIELDPC_IDX: usize = 3;
const PSR_IDX: usize = 4;
const PSP_IDX: usize = 5;
const REGS_IDX: usize = 6;
const REGS_RANGE: Range<usize> = REGS_IDX..REGS_IDX + 8;

const USIZE_SZ: usize = size_of::<usize>();
fn usize_byte_range(index: usize) -> Range<usize> {
    index * USIZE_SZ..(index + 1) * USIZE_SZ
}

fn usize_from_u8_slice(slice: &[u8], index: usize) -> Result<usize, ErrorCode> {
    let range = usize_byte_range(index);
    Ok(usize::from_le_bytes(
        slice
            .get(range)
            .ok_or(ErrorCode::SIZE)?
            .try_into()
            .or(Err(ErrorCode::FAIL))?,
    ))
}

fn write_usize_to_u8_slice(val: usize, slice: &mut [u8], index: usize) {
    let range = usize_byte_range(index);
    slice[range].copy_from_slice(&val.to_le_bytes());
}

impl core::convert::TryFrom<&[u8]> for CortexMStoredState {
    type Error = ErrorCode;
    fn try_from(ss: &[u8]) -> Result<CortexMStoredState, Self::Error> {
        if ss.len() == size_of::<CortexMStoredState>() + METADATA_LEN * USIZE_SZ
            && usize_from_u8_slice(ss, VERSION_IDX)? == VERSION
            && usize_from_u8_slice(ss, SIZE_IDX)? == STORED_STATE_SIZE
            && usize_from_u8_slice(ss, TAG_IDX)? == u32::from_le_bytes(TAG) as usize
        {
            let mut res = CortexMStoredState {
                regs: [0; 8],
                yield_pc: usize_from_u8_slice(ss, YIELDPC_IDX)?,
                psr: usize_from_u8_slice(ss, PSR_IDX)?,
                psp: usize_from_u8_slice(ss, PSP_IDX)?,
            };
            for (i, v) in (REGS_RANGE).enumerate() {
                res.regs[i] = usize_from_u8_slice(ss, v)?;
            }
            Ok(res)
        } else {
            Err(ErrorCode::FAIL)
        }
    }
}

/// Implementation of the `UserspaceKernelBoundary` for the Cortex-M non-floating point
/// architecture.
pub struct SysCall<A: CortexMVariant>(PhantomData<A>);

impl<A: CortexMVariant> SysCall<A> {
    pub const unsafe fn new() -> SysCall<A> {
        SysCall(PhantomData)
    }
}

impl<A: CortexMVariant> kernel::syscall::UserspaceKernelBoundary for SysCall<A> {
    type StoredState = CortexMStoredState;

    fn initial_process_app_brk_size(&self) -> usize {
        // Cortex-M hardware uses 8 words on the stack to implement context
        // switches. So we need at least 32 bytes.
        SVC_FRAME_SIZE
    }

    unsafe fn initialize_process(
        &self,
        accessible_memory_start: *const u8,
        app_brk: *const u8,
        state: &mut Self::StoredState,
    ) -> Result<(), ()> {
        // We need to initialize the stored state for the process here. This
        // initialization can be called multiple times for a process, for
        // example if the process is restarted.
        state.regs.iter_mut().for_each(|x| *x = 0);
        state.yield_pc = 0;
        state.psr = 0x01000000; // Set the Thumb bit and clear everything else.
        state.psp = app_brk as usize; // Set to top of process-accessible memory.

        // Make sure there's enough room on the stack for the initial SVC frame.
        if (app_brk as usize - accessible_memory_start as usize) < SVC_FRAME_SIZE {
            // Not enough room on the stack to add a frame.
            return Err(());
        }

        // Allocate the kernel frame
        state.psp -= SVC_FRAME_SIZE;
        Ok(())
    }

    unsafe fn update_stack_pointer(
        &self,
        new_stack_pointer: *const usize,
        state: &mut Self::StoredState,
    ) {
        state.psp = new_stack_pointer as usize;
    }

    /// Updates process registers after a syscall occurs
    unsafe fn update_process_registers(
        &self,
        new_process_regs: [usize; 8],
        state: &mut Self::StoredState,
    ) {
        state.regs = new_process_regs;
    }

    /// Updates yield pc after a syscall occurs
    unsafe fn update_yield_pc(&self, new_yield_pc: usize, state: &mut Self::StoredState) {
        state.yield_pc = new_yield_pc;
    }

    /// Updates process registers after a syscall occurs
    unsafe fn update_psr(&self, new_psr: usize, state: &mut Self::StoredState) {
        state.psr = new_psr;
    }

    unsafe fn set_syscall_return_value(
        &self,
        accessible_memory_start: *const u8,
        app_brk: *const u8,
        state: &mut Self::StoredState,
        return_value: kernel::syscall::SyscallReturn,
    ) -> Result<(), ()> {
        // For the Cortex-M arch, write the return values in the same
        // place that they were originally passed in (i.e. at the
        // bottom the SVC structure on the stack)

        // First, we need to validate that this location is inside of the
        // process's accessible memory. Alignment is guaranteed by hardware.
        if state.psp < accessible_memory_start as usize
            || state.psp.saturating_add(mem::size_of::<u32>() * 4) > app_brk as usize
        {
            return Err(());
        }

        let sp = state.psp as *mut u32;
        let (r0, r1, r2, r3) = (sp.offset(0), sp.offset(1), sp.offset(2), sp.offset(3));

        // These operations are only safe so long as
        // - the pointers are properly aligned. This is guaranteed because the
        //   pointers are all offset multiples of 4 bytes from the stack
        //   pointer, which is guaranteed to be properly aligned after
        //   exception entry on Cortex-M. See
        //   https://github.com/tock/tock/pull/2478#issuecomment-796389747
        //   for more details.
        // - the pointer is dereferencable, i.e. the memory range of
        //   the given size starting at the pointer must all be within
        //   the bounds of a single allocated object
        // - the pointer must point to an initialized instance of its
        //   type
        // - during the lifetime of the returned reference (of the
        //   cast, essentially an arbitrary 'a), the memory must not
        //   get accessed (read or written) through any other pointer.
        //
        // Refer to
        // https://doc.rust-lang.org/std/primitive.pointer.html#safety-13
        return_value.encode_syscall_return(&mut *r0, &mut *r1, &mut *r2, &mut *r3);

        Ok(())
    }

    /// When the process calls `svc` to enter the kernel, the hardware
    /// automatically pushes an SVC frame that will be unstacked when the kernel
    /// returns to the process. In the special case of process startup,
    /// `initialize_new_process` sets up an empty SVC frame as if an `svc` had
    /// been called.
    ///
    /// Here, we modify this stack frame such that the process resumes at the
    /// beginning of the callback function that we want the process to run. We
    /// place the originally intended return address in the link register so
    /// that when the function completes execution continues.
    ///
    /// In effect, this converts `svc` into `bl callback`.
    unsafe fn set_process_function(
        &self,
        accessible_memory_start: *const u8,
        app_brk: *const u8,
        state: &mut CortexMStoredState,
        callback: kernel::process::FunctionCall,
    ) -> Result<(), ()> {
        // Ensure that [`state.psp`, `state.psp + SVC_FRAME_SIZE`] is within
        // process-accessible memory. Alignment is guaranteed by hardware.
        if state.psp < accessible_memory_start as usize
            || state.psp.saturating_add(SVC_FRAME_SIZE) > app_brk as usize
        {
            return Err(());
        }

        // Notes:
        //  - Instruction addresses require `|1` to indicate thumb code
        //  - Stack offset 4 is R12, which the syscall interface ignores
        let stack_bottom = state.psp as *mut usize;
        ptr::write(stack_bottom.offset(7), state.psr); //......... -> APSR
        ptr::write(stack_bottom.offset(6), callback.pc | 1); //... -> PC
        ptr::write(stack_bottom.offset(5), state.yield_pc | 1); // -> LR
        ptr::write(stack_bottom.offset(3), callback.argument3); // -> R3
        ptr::write(stack_bottom.offset(2), callback.argument2); // -> R2
        ptr::write(stack_bottom.offset(1), callback.argument1); // -> R1
        ptr::write(stack_bottom.offset(0), callback.argument0); // -> R0

        Ok(())
    }

    unsafe fn switch_to_process(
        &self,
        accessible_memory_start: *const u8,
        app_brk: *const u8,
        state: &mut CortexMStoredState,
    ) -> (kernel::syscall::ContextSwitchReason, Option<*const u8>) {
        let new_stack_pointer = A::switch_to_user(state.psp as *const usize, &mut state.regs);

        // We need to keep track of the current stack pointer.
        state.psp = new_stack_pointer as usize;

        // We need to validate that the stack pointer and the SVC frame are
        // within process accessible memory. Alignment is guaranteed by
        // hardware.
        let invalid_stack_pointer = state.psp < accessible_memory_start as usize
            || state.psp.saturating_add(SVC_FRAME_SIZE) > app_brk as usize;

        // Determine why this returned and the process switched back to the
        // kernel.

        // Check to see if the fault handler was called while the process was
        // running.
        let app_fault = read_volatile(&APP_HARD_FAULT);
        write_volatile(&mut APP_HARD_FAULT, 0);

        // Check to see if the svc_handler was called and the process called a
        // syscall.
        let syscall_fired = read_volatile(&SYSCALL_FIRED);
        write_volatile(&mut SYSCALL_FIRED, 0);

        // Now decide the reason based on which flags were set.
        let switch_reason = if app_fault == 1 || invalid_stack_pointer {
            // APP_HARD_FAULT takes priority. This means we hit the hardfault
            // handler and this process faulted.
            kernel::syscall::ContextSwitchReason::Fault
        } else if syscall_fired == 1 {
            // Save these fields after a syscall. If this is a synchronous
            // syscall (i.e. we return a value to the app immediately) then this
            // will have no effect. If we are doing something like `yield()`,
            // however, then we need to have this state.
            state.yield_pc = ptr::read(new_stack_pointer.offset(6));
            state.psr = ptr::read(new_stack_pointer.offset(7));

            // Get the syscall arguments and return them along with the syscall.
            // It's possible the app did something invalid, in which case we put
            // the app in the fault state.
            let r0 = ptr::read(new_stack_pointer.offset(0));
            let r1 = ptr::read(new_stack_pointer.offset(1));
            let r2 = ptr::read(new_stack_pointer.offset(2));
            let r3 = ptr::read(new_stack_pointer.offset(3));

            // Get the actual SVC number.
            let pcptr = ptr::read((new_stack_pointer as *const *const u16).offset(6));
            let svc_instr = ptr::read(pcptr.offset(-1));
            let svc_num = (svc_instr & 0xff) as u8;

            // Use the helper function to convert these raw values into a Tock
            // `Syscall` type.
            let syscall =
                kernel::syscall::Syscall::from_register_arguments(svc_num, r0, r1, r2, r3);

            match syscall {
                Some(s) => kernel::syscall::ContextSwitchReason::SyscallFired { syscall: s },
                None => kernel::syscall::ContextSwitchReason::Fault,
            }
        } else {
            // If none of the above cases are true its because the process was interrupted by an
            // ISR for a hardware event
            kernel::syscall::ContextSwitchReason::Interrupted
        };

        (switch_reason, Some(new_stack_pointer as *const u8))
    }

    unsafe fn new_switch_to_process<C: Chip>(
        &self,
        chip: &C,
        process_id: process::ProcessId,
        // process: &dyn process::Process,
        state: &mut CortexMStoredState,
    ) {
        unsafe {
            chip.atomic(|| {
                // Update PSP and ProcessRegs global variables
                // These variables will be updated by the SVC handler
                PROCESS_STACK_POINTER = state.psp as *const usize;
                PROCESS_REGS = state.regs;

                // Update ProcessId global variable
                // This is necessary to retrieve the process in order to process its SVC call in handler mode
                PROCESS_ID = (&process_id as *const _) as *const usize;
                // PROCESS = process;

                // Save Process State (CortexMSavedState)
                PROCESS_STATE = (state as *const _) as *const usize;

                // debug!(
                //     "In process.new_switch_to_process() for process {}\n",
                //     process_id.id()
                // );

                // Set the PendSV bit to switch to userspace
                scb::set_pendsv();
            });
        }
    }

    unsafe extern "C" fn handle_svc_call<KR: KernelResources<C>, C: Chip>(
        resources: &KR,
        curr_process: &process::ProcessId,
    ) {
        if COUNTER == 0 {
            dwt::reset_timer();
            dwt::start_timer();
        } else if COUNTER == 100 {
            dwt::stop_timer();
            let num_cycles = dwt::get_time();
            panic!("Num cycles is {}", num_cycles)
        }
        COUNTER += 1;

        let process = curr_process.get_process();
        let state = unsafe { &mut *(PROCESS_STATE as *mut CortexMStoredState) };

        // We need to validate that the stack pointer and the SVC frame are
        // within process accessible memory. Alignment is guaranteed by
        // hardware.
        match process {
            Some(p) => {
                // debug!("Found process {}\n", p.get_process_name());

                // Update process state with current stack pointer and process registers.
                let new_stack_pointer = read_volatile(&PROCESS_STACK_POINTER);
                state.psp = new_stack_pointer as usize;

                let new_process_regs = read_volatile(&PROCESS_REGS);
                state.regs = new_process_regs;

                // Check for invalid stack pointer
                let process_info = p.get_addresses();
                let accessible_memory_start = process_info.sram_start;
                let app_brk = process_info.sram_app_brk;

                let sp_usize = new_stack_pointer as usize;
                let invalid_stack_pointer = sp_usize < accessible_memory_start as usize
                    || sp_usize.saturating_add(SVC_FRAME_SIZE) > app_brk as usize;

                if invalid_stack_pointer {
                    // Process faulted, set fault state
                    p.set_fault_state();
                }

                // Save these fields after a syscall. If this is a synchronous
                // syscall (i.e. we return a value to the app immediately) then this
                // will have no effect. If we are doing something like `yield()`,
                // however, then we need to have this state.
                state.yield_pc = ptr::read(new_stack_pointer.offset(6));
                state.psr = ptr::read(new_stack_pointer.offset(7));

                // Get the syscall arguments and return them along with the syscall.
                // It's possible the app did something invalid, in which case we put
                // the app in the fault state.
                let r0 = ptr::read(new_stack_pointer.offset(0));
                let r1 = ptr::read(new_stack_pointer.offset(1));
                let r2 = ptr::read(new_stack_pointer.offset(2));
                let r3 = ptr::read(new_stack_pointer.offset(3));

                // Get the actual SVC number.
                let pcptr = ptr::read((new_stack_pointer as *const *const u16).offset(6));
                let svc_instr = ptr::read(pcptr.offset(-1));
                let svc_num = (svc_instr & 0xff) as u8;

                // Use the helper function to convert these raw values into a Tock
                // `Syscall` type.
                let syscall =
                    kernel::syscall::Syscall::from_register_arguments(svc_num, r0, r1, r2, r3);

                // Call the kernel's handle_syscall function
                match syscall {
                    Some(s) => {
                        kernel::Kernel::handle_syscall(resources, p, s);
                    }
                    None => {
                        p.set_fault_state();
                    }
                };
            }
            // This case shouldn't happen
            _ => debug!("Something went wrong: process that called SVC not found"),
        };

        // Return back to the svc_handler
    }

    unsafe fn print_context(
        &self,
        accessible_memory_start: *const u8,
        app_brk: *const u8,
        state: &CortexMStoredState,
        writer: &mut dyn Write,
    ) {
        // Check if the stored stack pointer is valid. Alignment is guaranteed
        // by hardware.
        let invalid_stack_pointer = state.psp < accessible_memory_start as usize
            || state.psp.saturating_add(SVC_FRAME_SIZE) > app_brk as usize;

        let stack_pointer = state.psp as *const usize;

        // If we cannot use the stack pointer, generate default bad looking
        // values we can use for the printout. Otherwise, read the correct
        // values.
        let (r0, r1, r2, r3, r12, lr, pc, xpsr) = if invalid_stack_pointer {
            (
                0xBAD00BAD, 0xBAD00BAD, 0xBAD00BAD, 0xBAD00BAD, 0xBAD00BAD, 0xBAD00BAD, 0xBAD00BAD,
                0xBAD00BAD,
            )
        } else {
            let r0 = ptr::read(stack_pointer.offset(0));
            let r1 = ptr::read(stack_pointer.offset(1));
            let r2 = ptr::read(stack_pointer.offset(2));
            let r3 = ptr::read(stack_pointer.offset(3));
            let r12 = ptr::read(stack_pointer.offset(4));
            let lr = ptr::read(stack_pointer.offset(5));
            let pc = ptr::read(stack_pointer.offset(6));
            let xpsr = ptr::read(stack_pointer.offset(7));
            (r0, r1, r2, r3, r12, lr, pc, xpsr)
        };

        let _ = writer.write_fmt(format_args!(
            "\
             \r\n  R0 : {:#010X}    R6 : {:#010X}\
             \r\n  R1 : {:#010X}    R7 : {:#010X}\
             \r\n  R2 : {:#010X}    R8 : {:#010X}\
             \r\n  R3 : {:#010X}    R10: {:#010X}\
             \r\n  R4 : {:#010X}    R11: {:#010X}\
             \r\n  R5 : {:#010X}    R12: {:#010X}\
             \r\n  R9 : {:#010X} (Static Base Register)\
             \r\n  SP : {:#010X} (Process Stack Pointer)\
             \r\n  LR : {:#010X}\
             \r\n  PC : {:#010X}\
             \r\n YPC : {:#010X}\
             \r\n",
            r0,
            state.regs[2],
            r1,
            state.regs[3],
            r2,
            state.regs[4],
            r3,
            state.regs[6],
            state.regs[0],
            state.regs[7],
            state.regs[1],
            r12,
            state.regs[5],
            stack_pointer as usize,
            lr,
            pc,
            state.yield_pc,
        ));
        let _ = writer.write_fmt(format_args!(
            "\
             \r\n APSR: N {} Z {} C {} V {} Q {}\
             \r\n       GE {} {} {} {}",
            (xpsr >> 31) & 0x1,
            (xpsr >> 30) & 0x1,
            (xpsr >> 29) & 0x1,
            (xpsr >> 28) & 0x1,
            (xpsr >> 27) & 0x1,
            (xpsr >> 19) & 0x1,
            (xpsr >> 18) & 0x1,
            (xpsr >> 17) & 0x1,
            (xpsr >> 16) & 0x1,
        ));
        let ici_it = (((xpsr >> 25) & 0x3) << 6) | ((xpsr >> 10) & 0x3f);
        let thumb_bit = ((xpsr >> 24) & 0x1) == 1;
        let _ = writer.write_fmt(format_args!(
            "\
             \r\n EPSR: ICI.IT {:#04x}\
             \r\n       ThumbBit {} {}\r\n",
            ici_it,
            thumb_bit,
            if thumb_bit {
                ""
            } else {
                "!!ERROR - Cortex M Thumb only!"
            },
        ));
    }

    fn store_context(
        &self,
        state: &CortexMStoredState,
        out: &mut [u8],
    ) -> Result<usize, ErrorCode> {
        if out.len() >= size_of::<CortexMStoredState>() + 3 * USIZE_SZ {
            write_usize_to_u8_slice(VERSION, out, VERSION_IDX);
            write_usize_to_u8_slice(STORED_STATE_SIZE, out, SIZE_IDX);
            write_usize_to_u8_slice(u32::from_le_bytes(TAG) as usize, out, TAG_IDX);
            write_usize_to_u8_slice(state.yield_pc, out, YIELDPC_IDX);
            write_usize_to_u8_slice(state.psr, out, PSR_IDX);
            write_usize_to_u8_slice(state.psp, out, PSP_IDX);
            for (i, v) in state.regs.iter().enumerate() {
                write_usize_to_u8_slice(*v, out, REGS_IDX + i);
            }
            // + 3 for yield_pc, psr, psp
            Ok((state.regs.len() + 3 + METADATA_LEN) * USIZE_SZ)
        } else {
            Err(ErrorCode::SIZE)
        }
    }
}
