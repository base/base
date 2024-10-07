# Multithreaded Cannon Fault Proof Virtual Machine

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [New Features](#new-features)
    - [Multithreading](#multithreading)
    - [Robustness](#robustness)
- [Multithreading](#multithreading-1)
  - [Thread Management](#thread-management)
  - [Thread Traversal Mechanics](#thread-traversal-mechanics)
    - [Thread Preemption](#thread-preemption)
  - [Wakeup Traversal](#wakeup-traversal)
  - [Exited Threads](#exited-threads)
  - [Waiting Threads](#waiting-threads)
  - [Voluntary Preemption](#voluntary-preemption)
  - [Forced Preemption](#forced-preemption)
- [Stateful Instructions](#stateful-instructions)
- [FPVM State](#fpvm-state)
  - [State](#state)
  - [State Hash](#state-hash)
  - [Thread State](#thread-state)
  - [Thread Hash](#thread-hash)
  - [Thread Stack Hashing](#thread-stack-hashing)
- [Memory](#memory)
  - [Heap](#heap)
- [Delay Slots](#delay-slots)
- [Syscalls](#syscalls)
  - [Supported Syscalls](#supported-syscalls)
  - [Noop Syscalls](#noop-syscalls)
- [I/O](#io)
  - [Standard Streams](#standard-streams)
  - [Hint Communication](#hint-communication)
  - [Pre-image Communication](#pre-image-communication)
    - [Pre-image I/O Alignment](#pre-image-io-alignment)
- [Exceptions](#exceptions)
- [Security Model](#security-model)
  - [Compiler Correctness](#compiler-correctness)
  - [Compiler Assumptions](#compiler-assumptions)

<!-- END doctoc generated TOC please keep comment here to allow auto update -->

## Overview

This is a description of the second iteration of the Cannon Fault Proof Virtual Machine (FPVM).
When necessary to distinguish this version from the initial implementation,
it can be referred to as Multithreaded Cannon (MTCannon). Similarly,
the original Cannon implementation can be referred to as Singlethreaded Cannon (STCannon) where necessary for clarity.

The MTCannon FPVM emulates a minimal uniprocessor Linux-based system running on big-endian 32-bit MIPS32 architecture.
A lot of its behaviors are copied from Linux/MIPS with a few tweaks made for fault proofs.
For the rest of this doc, we refer to the MTCannon FPVM as simply the FPVM.

Operationally, the FPVM is a state transition function. This state transition is referred to as a _Step_,
that executes a single instruction. We say the VM is a function $f$, given an input state $S_{pre}$, steps on a
single instruction encoded in the state to produce a new state $S_{post}$.
$$f(S_{pre}) \rightarrow S_{post}$$

Thus, the trace of a program executed by the FPVM is an ordered set of VM states.

### New Features

#### Multithreading

MTCannon adds support for [multithreading](https://en.wikipedia.org/wiki/Thread_(computing)).
Thread management and scheduling are typically handled by the
[operating system (OS) kernel](https://en.wikipedia.org/wiki/Kernel_%28operating_system%29):
programs make thread-related requests to the OS kernel via [syscalls](https://en.wikipedia.org/wiki/System_call).
As such, this implementation includes a few new Linux-specific thread-related [syscalls](#syscalls).
Additionally, the [FPVM state](#fpvm-state) has been modified in order to track the set of active threads
and thread-related global state.

#### Robustness

In the initial implementation of Cannon, unrecognized syscalls were treated as
noops (see ["Noop Syscalls"](#noop-syscalls)). To ensure no unexpected behaviors are triggered,
MTCannon will now raise an exception if unrecognized syscalls are encountered during program execution.

## Multithreading

The MTCannon FPVM rotates between threads to provide
[multitasking](https://en.wikipedia.org/wiki/Computer_multitasking) rather than
true [parallel processing](https://en.wikipedia.org/wiki/Parallel_computing).
The VM state holds an ordered set of thread state objects representing all executing threads.

On any given step, there is one active thread that will be processed.

### Thread Management

The FPVM state contains two thread stacks that are used to represent the set of all threads: `leftThreadStack` and
`rightThreadStack`. An additional boolean value (`traverseRight`) determines which stack contains the currently active
thread and how threads are rearranged when the active thread is preempted (see ["Thread Preemption"](#thread-preemption)
for details).

When traversing right, the thread on the top of the right stack is the active thread, the right stack is referred to as
the "active" stack,
and the left the "inactive" stack.  Conversely, when traversing left, the active thread is on top of the left stack,
the left stack is "active", and the right is "inactive".

Representing the set of threads as two stacks allows for a succinct commitment to the contents of all threads.
For details, see [“Thread Stack Hashing”](#thread-stack-hashing).

### Thread Traversal Mechanics

Threads are traversed deterministically by moving from the first thread to the last thread,
then from the last thread to the first thread repeatedly.  For example, given the set of threads: {0,1,2,3},
the FPVM would traverse to each as follows: 0, 1, 2, 3, 3, 2, 1, 0, 0, 1, 2, 3, 3, 2, ….

#### Thread Preemption

Threads are traversed via "preemption": the currently active thread is popped from the active stack and pushed to the
inactive stack.  If the active stack is empty, the FPVM state's `traverseRight` field is flipped ensuring that
there is always an active thread.

### Wakeup Traversal

When a futex wake syscall is made, the FPVM state’s `wakeup` field is set to the memory address specified by this
syscall.  This causes the FPVM to enter a "wakeup traversal" mode where it iterates
through the existing threads, looking for a thread that is currently waiting on the `wakeup` address.
The wakeup traversal will continue until such a thread is found or else all threads have been checked.
During wakeup traversal, no instructions are processed and no threads are updated, the VM simply steps through threads
one at a time until wakeup traversal completes.

Wakeup traversal proceeds as follows across multiple steps:

- When a futex wakeup syscall is made:
  - The state's `wakeup` field is set to an address specified by the syscall.
  - The currently active thread is preempted.
  - The FPVM state is set to traverse left, if possible (if the left thread stack is non-empty).
- On each subsequent step while `wakeup` is set:
  - The currently active thread's `futexAddr` is checked for a match with `wakeup`.
  - If a match is found:
    - The wakeup traversal completes [^traversal-completion], leaving the matching thread as the currently active thread.
  - If the currently active thread is not a match:
    - The active thread is preempted.
    - If the right thread stack is now empty:
      - This means all threads have been visited (the traversal begins by moving
        left, then right so this is the end of the traversal).
      - The wakeup traversal completes [^traversal-completion].

[^traversal-completion]: Wakeup traversal is completed by setting the FPVM state's `wakeup` field to `0xFFFFFFFF` (-1),
causing the FPVM to resume normal execution.

### Exited Threads

When the VM encounters an active thread that has exited, it is popped from the active thread stack, removing it from
the VM state.

### Waiting Threads

Threads enter a waiting state when a futex wait syscall is successfully executed, setting the thread's
`futexAddr`, `futexVal`, and `futexTimeoutStep` fields according to the futex syscall arguments.

During normal execution, when the active thread is in a waiting state (its `futexAddr` is not `0xFFFFFFFF`), the VM
checks if it can be woken up.

A waiting thread will be woken up if:

- The current `step` (after incrementing) is greater than `futexTimeoutStep`
- The memory value at `futexAddr` is no longer equal to `futexVal`

The VM will wake such a thread by resetting its futex fields:

- `futexAddr` = `0xFFFFFFFF`
- `futexVal` = 0
- `futexTimeoutStep` = 0

If the current thread is waiting and cannot be woken, it is preempted.

### Voluntary Preemption

In addition to the futex wait syscall (see ["Waiting Threads"](#waiting-threads)), there are a few other syscalls that
will cause a thread to be "voluntarily" preempted: `sched_yield`, `nanosleep`.

### Forced Preemption

To avoid thread starvation (for example where a thread hogs resources by never executing a sleep, yield, or wait),
the FPVM will force a context switch if the active thread has been executing too long.

For each step executed on a particular thread, the state field `stepsSinceLastContextSwitch` is incremented.
When a thread is preempted, `StepsSinceLastContextSwitch` is reset to 0.
If `StepsSinceLastContextSwitch` reaches a maximum value (`SchedQuantum` = 100_000),
the FPVM preempts the active thread.

## Stateful Instructions

The Load Linked Word (`ll`) and Store Conditional Word (`sc`) instructions provide the low-level
primitives used to implement atomic read-modify-write (RMW) operations.  A typical RMW sequence might play out as
follows:

- `ll` place a "reservation" on a particular memory address.
- Subsequent instructions take the value at this address and perform some operation on it:
  - For example, maybe a counter variable is reserved and incremented.
- `sc` is called and the modified value is stored at the reserved address only if it has not been modified since the
reservation was placed.

This RMW sequence ensures that if another thread or process modifies a target memory address while
an atomic update is being performed, the atomic update will fail.

Prior to MTCannon, we could be assured that no intervening process would modify that target memory location because
STCannon is singlethreaded.  With the introduction of multithreading, additional fields need to be stored in the
FPVM state to track memory reservations initiated by `ll` operations.

When an `ll` instruction is executed:

- `llReservationActive` is set to true.
- `llAddress` is set to the memory address specified by `ll`.
- `llOwnerThread` is set to the `threadID` of the active thread.

Only a single memory reservation can be active at a given time - a new reservation will clear any previous reservation.

When the VM writes any data to memory, these `ll`-related fields are checked and the memory reservation is cleared if
a memory write touches a reserved `llAddress`.

When an `sc` instruction is executed, the operation will only succeed if:

- There exists an active reservation (`llReservationActive == true`).
- The active thread's `threadID` matches `llOwnerThread`.
- The requested address matches `llAddress`.

On success, `sc` stores a value at the target memory address, clears the memory reservation and returns `1`.
On failure, `sc` returns `0`.

## FPVM State

### State

The FPVM is a state transition function that operates on a state object consisting of the following fields:

1. `memRoot` - A `bytes32` value representing the merkle root of VM memory.
1. `preimageKey` - `bytes32` value of the last requested pre-image key.
1. `preimageOffset` - The 32-bit value of the last requested pre-image offset.
1. `heap` - 32-bit base address of the most recent memory allocation via mmap.
1. `llReservationActive` - 8-bit boolean indicator of whether a memory reservation,
   which is reserved via a Load Linked Word (`ll`) instruction, is active.
1. `llAddress` - 32-bit address of the currently active memory reservation if one exists.
1. `llOwnerThread` - 32-bit id of the thread that initiated the current memory reservation if one exists.
1. `exitCode` - 8-bit exit code.
1. `exited` - 8-bit boolean valuel indicating whether the VM has exited.
1. `step` - 64-bit step counter.
1. `stepsSinceLastContextSwitch` - 64-bit step counter that tracks the number of steps executed on the current
   thread since the last [preemption](#thread-preemption).
1. `wakeup` - 32-bit address set via a futex syscall signaling that the VM has entered wakeup traversal or else
    `0xFFFFFFFF` (-1) if there is no active wakeup signal. For details see ["Wakeup Traversal"](#wakeup-traversal).
1. `traverseRight` - 8-bit boolean that indicates whether the currently active thread is on the left or right thread
    stack, as well as some details on thread traversal mechanics.
    See ["Thread Traversal Mechanics"](#thread-traversal-mechanics) for details.
1. `leftThreadStack` - a `bytes32` hash of the contents of the left thread stack.
   For details, see the [“Thread Stack Hashing” section.](#thread-stack-hashing)
1. `rightThreadStack` - a `bytes32` hash of the contents of the right thread stack.
   For details, see the [“Thread Stack Hashing” section.](#thread-stack-hashing)
1. `nextThreadID` - 32-bit value defining the id to assign to the next thread that is created.

The state is represented by packing the above fields, in order, into a 172-byte buffer.

### State Hash

The state hash is computed by hashing the 172-byte state buffer with the Keccak256 hash function
and then setting the high-order byte to the respective VM status.

The VM status can be derived from the state's `exited` and `exitCode` fields.

```rs
enum VmStatus {
    Valid = 0,
    Invalid = 1,
    Panic = 2,
    Unfinished = 3,
}

fn vm_status(exit_code: u8, exited: bool) -> u8 {
    if exited {
        match exit_code {
            0 => VmStatus::Valid,
            1 => VmStatus::Invalid,
            _ => VmStatus::Panic,
        }
    } else {
        VmStatus::Unfinished
    }
}
```

### Thread State

The state of a single thread is tracked and represented by a thread state object consisting of the following fields:

1. `threadID` - 32-bit unique thread identifier.
1. `exitCode` - 8-bit exit code.
1. `exited` - 8-bit boolean value indicating whether the thread has exited.
1. `futexAddr` - 32-bit address set via a futex syscall indicating that this thread is waiting on a value change
    at this address.
1. `futexVal` - 32-bit value representing the memory contents at `futexAddr` when this thread began waiting.
1. `futexTimeoutStep` - 64-bit value representing the future `step` at which the futex wait will time out.  Set to the
   max uint64 value (-1) if no timeout is active.
1. `pc` - 32-bit program counter.
1. `nextPC` - 32-bit next program counter. Note that this value may not always be $pc+4$
   when executing a branch/jump delay slot.
1. `lo` - 32-bit MIPS LO special register.
1. `hi` - 32-bit MIPS HI special register.
1. `registers` - General-purpose MIPS32 registers. Each register is a 32-bit value.

A thread is represented by packing the above fields, in order, into a 166-byte buffer.

### Thread Hash

A thread hash is computed by hashing the 166-byte thread state buffer with the Keccak256 hash function.

### Thread Stack Hashing

> **Note:** The `++` operation represents concatenation of 2 byte string arguments

Each thread stack is represented in the FPVM state by a "hash onion" construction using the Keccak256 hash
function. This construction provides a succinct commitment to the contents of a thread stack using a single `bytes32`
value:

- An empty stack is represented by the value:
  - `c0 = hash(bytes32(0) ++ bytes32(0))`
- To push a thread to the stack, hash the concatenation of the current stack commitment with the thread hash:
  - `push(c0, el0) => c1 = hash(c0 ++ hash(el0))`.
- To push another thread:
  - `push(c1, el1) => c2 = hash(c1 ++ hash(el1))`.
- To pop an element from the stack, peel back the last hash (push) operation:
  - `pop(c2) => c3 = c1`
- To prove the top value `elTop` on the stack, given some commitment `c`, you just need to reveal the `bytes32`
  commitment `c'` for the stack without `elTop` and verify:
  - `c = hash(c' ++ hash(elTop))`

## Memory

Memory is represented as a binary merkle tree.
The tree has a fixed-depth of 27 levels, with leaf values of 32 bytes each.
This spans the full 32-bit address space, where each leaf contains the memory at that part of the tree.
The state `memRoot` represents the merkle root of the tree, reflecting the effects of memory writes.
As a result of this memory representation, all memory operations are 4-byte aligned.
Memory access doesn't require any privileges. An instruction step can access any memory
location as the entire address space is unprotected.

### Heap

FPVM state contains a `heap` that tracks the base address of the most recent memory allocation.
Heap pages are bump allocated at the page boundary, per `mmap` syscall.
mmap-ing is purely to satisfy program runtimes that need the memory-pointer
result of the syscall to locate free memory. The page size is 4096.

The FPVM has a fixed program break at `0x40000000`. However, the FPVM is permitted to extend the
heap beyond this limit via mmap syscalls.
For simplicity, there are no memory protections against "heap overruns" against other memory segments.
Such VM steps are still considered valid state transitions.

Specification of memory mappings is outside the scope of this document as it is irrelevant to
the VM state. FPVM implementers may refer to the Linux/MIPS kernel for inspiration.

## Delay Slots

The post-state of a step updates the `nextPC`, indicating the instruction following the `pc`.
However, in the case of where a branch instruction is being stepped, the `nextPC` post-state is
set to the branch target. And the `pc` post-state set to the branch delay slot as usual.

A VM state transition is invalid whenever the current instruction is a delay slot that is filled
with jump or branch type instruction.
That is, where $nextPC \neq pc + 4$ while stepping on a jump/branch instruction.
Otherwise, there would be two consecutive delay slots. While this is considered "undefined"
behavior in typical MIPS implementations, FPVM must raise an exception when stepping on such states.

## Syscalls

Syscalls work similar to [Linux/MIPS](https://www.linux-mips.org/wiki/Syscall), including the
syscall calling conventions and general syscall handling behavior.
However, the FPVM supports a subset of Linux/MIPS syscalls with slightly different behaviors.
These syscalls have identical syscall numbers and ABIs as Linux/MIPS.

For all of the following syscalls, an error is indicated by setting the return
register (`$v0`) to `0xFFFFFFFF` (-1) and `errno` (`$a3`) is set accordingly.
The VM must not modify any register other than `$v0` and `$a3` during syscall handling.

The following tables summarize supported syscalls and their behaviors.
If an unsupported syscall is encountered, the VM will raise an exception.

### Supported Syscalls

| \$v0 | system call   | \$a0            | \$a1             | \$a2         | \$a3             | Effect                                                                                                                                                                                                                                                                                                                                                                                                                                             |
|------|---------------|-----------------|------------------|--------------|------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 4090 | mmap          | uint32 addr     | uint32 len       | 🚫           | 🚫               | Allocates a page from the heap. See [heap](#heap) for details.                                                                                                                                                                                                                                                                                                                                                                                     |
| 4045 | brk           | 🚫              | 🚫               | 🚫           | 🚫               | Returns a fixed address for the program break at `0x40000000`                                                                                                                                                                                                                                                                                                                                                                                      |
| 4246 | exit_group    | uint8 exit_code | 🚫               | 🚫           | 🚫               | Sets the exited and exitCode state fields to `true` and `$a0` respectively.                                                                                                                                                                                                                                                                                                                                                                        |
| 4003 | read          | uint32 fd       | char \*buf       | uint32 count | 🚫               | Similar behavior as Linux/MIPS with support for unaligned reads. See [I/O](#io) for more details.                                                                                                                                                                                                                                                                                                                                                  |
| 4004 | write         | uint32 fd       | char \*buf       | uint32 count | 🚫               | Similar behavior as Linux/MIPS with support for unaligned writes. See [I/O](#io) for more details.                                                                                                                                                                                                                                                                                                                                                 |
| 4055 | fcntl         | uint32 fd       | int32 cmd        | 🚫           | 🚫               | Similar behavior as Linux/MIPS. Only the `F_GETFD`(1) and `F_GETFL` (3) cmds are supported. Sets errno to `0x16` for all other commands.                                                                                                                                                                                                                                                                                                           |
| 4120 | clone         | uint32 flags    | uint32 stack_ptr | 🚫           | 🚫               | Creates a new thread based on the currently active thread's state.  Supports a `flags` argument equal to `0x00050f00`, other values cause the VM to exit with exit_code `VmStatus.PANIC`.                                                                                                                                                                                                                                                          |
| 4001 | exit          | uint8 exit_code | 🚫               | 🚫           | 🚫               | Sets the active thread's exited and exitCode state fields to `true` and `$a0` respectively.                                                                                                                                                                                                                                                                                                                                                        |
| 4162 | sched_yield   | 🚫              | 🚫               | 🚫           | 🚫               | Preempts the active thread and returns 0.                                                                                                                                                                                                                                                                                                                                                                                                          |
| 4222 | gettid        | 🚫              | 🚫               | 🚫           | 🚫               | Returns the active thread's threadID field.                                                                                                                                                                                                                                                                                                                                                                                                        |
| 4238 | futex         | uint32 addr     | uint32 futex_op  | uint32 val   | uint32 \*timeout | Supports `futex_op`'s `FUTEX_WAIT_PRIVATE` (128) and `FUTEX_WAKE_PRIVATE` (129). Other operations set errno to `0x16`.                                                                                                                                                                                                                                                                                                                             |
| 4005 | open          | 🚫              | 🚫               | 🚫           | 🚫               | Sets errno to `0x9`.                                                                                                                                                                                                                                                                                                                                                                                                                               |
| 4166 | nanosleep     | 🚫              | 🚫               | 🚫           | 🚫               | Preempts the active thread and returns 0.                                                                                                                                                                                                                                                                                                                                                                                                          |
| 4263 | clock_gettime | uint32 clock_id | uint32 addr      | 🚫           | 🚫               | Supports `clock_id`'s `REALTIME`(0) and `MONOTONIC`(1). For other `clock_id`'s, sets errno to `0x16`.  Calculates a deterministic time value based on the state's `step` field and a constant `HZ` (10,000,000) where `HZ` represents the approximate clock rate (steps / second) of the FPVM:<br/><br/>`seconds = step/HZ`<br/>`nsecs = (step % HZ) * 10^9/HZ`<br/><br/>Seconds are set at memory address `addr` and nsecs are set at `addr + 4`. |
| 4020 | getpid        | 🚫              | 🚫               | 🚫           | 🚫               | Returns 0.                                                                                                                                                                                                                                                                                                                                                                                                                                         |

### Noop Syscalls

For the following noop syscalls, the VM must do nothing except to zero out the syscall return (`$v0`)
and errno (`$a3`) registers.

| \$v0 | system call        |
|------|--------------------|
| 4091 | munmap             |
| 4240 | sched_get_affinity |
| 4218 | madvise            |
| 4195 | rt_sigprocmask     |
| 4206 | sigaltstack        |
| 4194 | rt_sigaction       |
| 4338 | prlimit64          |
| 4006 | close              |
| 4200 | pread64            |
| 4108 | fstat              |
| 4215 | fstat64            |
| 4288 | openat             |
| 4085 | readlink           |
| 4298 | readlinkat         |
| 4054 | ioctl              |
| 4326 | epoll_create1      |
| 4328 | pipe2              |
| 4249 | epoll_ctl          |
| 4313 | epoll_pwait        |
| 4353 | getrandom          |
| 4122 | uname              |
| 4213 | stat64             |
| 4024 | getuid             |
| 4047 | getgid             |
| 4140 | llseek             |
| 4217 | mincore            |
| 4266 | tgkill             |
| 4104 | setitimer          |
| 4257 | timer_create       |
| 4258 | timer_settime      |
| 4261 | timer_delete       |

## I/O

The VM does not support Linux open(2). However, the VM can read from and write to a predefined set of file descriptors.

| Name               | File descriptor | Description                                                                  |
| ------------------ | --------------- | ---------------------------------------------------------------------------- |
| stdin              | 0               | read-only standard input stream.                                             |
| stdout             | 1               | write-only standard output stream.                                           |
| stderr             | 2               | write-only standard error stream.                                            |
| hint response      | 3               | read-only. Used to read the status of [pre-image hinting](../fault-proof/index.md#hinting). |
| hint request       | 4               | write-only. Used to provide [pre-image hints](../fault-proof/index.md#hinting)              |
| pre-image response | 5               | read-only. Used to [read pre-images](../fault-proof/index.md#pre-image-communication).      |
| pre-image request  | 6               | write-only. Used to [request pre-images](../fault-proof/index.md#pre-image-communication).  |

Syscalls referencing unknown file descriptors fail with an `EBADF` errno as done on Linux.

Writing to and reading from standard output, input and error streams have no effect on the FPVM state.
FPVM implementations may use them for debugging purposes as long as I/O is stateless.

All I/O operations are restricted to a maximum of 4 bytes per operation.
Any read or write syscall request exceeding this limit will be truncated to 4 bytes.
Consequently, the return value of read/write syscalls is at most 4, indicating the actual number of bytes read/written.

### Standard Streams

Writing to stderr/stdout standard stream always succeeds with the write count input returned,
effectively continuing execution without writing work.
Reading from stdin has no effect other than to return zero and errno set to 0, signalling that there is no input.

### Hint Communication

Hint requests and responses have no effect on the VM state other than setting the `$v0` return
register to the requested read/write count.
VM implementations may utilize hints to setup subsequent pre-image requests.

### Pre-image Communication

The `preimageKey` and `preimageOffset` state are updated via read/write syscalls to the pre-image
read and write file descriptors (see [I/O](#io)).
The `preimageKey` buffers the stream of bytes written to the pre-image write fd.
The `preimageKey` buffer is shifted to accommodate new bytes written to the end of it.
A write also resets the `preimageOffset` to 0, indicating the intent to read a new pre-image.

When handling pre-image reads, the `preimageKey` is used to lookup the pre-image data from an Oracle.
A max 4-byte chunk of the pre-image at the `preimageOffset` is read to the specified address.
Each read operation increases the `preimageOffset` by the number of bytes requested
(truncated to 4 bytes and subject to alignment constraints).

#### Pre-image I/O Alignment

As mentioned earlier in [memory](#memory), all memory operations are 4-byte aligned.
Since pre-image I/O occurs on memory, all pre-image I/O operations must strictly adhere to alignment boundaries.
This means the start and end of a read/write operation must fall within the same alignment boundary.
If an operation were to violate this, the input `count` of the read/write syscall must be
truncated such that the effective address of the last byte read/written matches the input effective address.

The VM must read/write the maximum amount of bytes possible without crossing the input address alignment boundary.
For example, the effect of a write request for a 3-byte aligned buffer must be exactly 3 bytes.
If the buffer is misaligned, then the VM may write less than 3 bytes depending on the size of the misalignment.

## Exceptions

The FPVM may raise an exception rather than output a post-state to signal an invalid state
transition. Nominally, the FPVM must raise an exception in at least the following cases:

- Invalid instruction (either via an invalid opcode or an instruction referencing registers
  outside the general purpose registers).
- Unsupported syscall.
- Pre-image read at an offset larger than the size of the pre-image.
- Delay slot contains branch/jump instruction types.
- Invalid thread state:
  - There are no threads - both thread stacks are empty.
  - The active thread stack is empty.

VM implementations may raise an exception in other cases that is specific to the implementation.
For example, an on-chain FPVM that relies on pre-supplied merkle proofs for memory access may
raise an exception if the supplied merkle proof does not match the pre-state `memRoot`.

## Security Model

### Compiler Correctness

Cannon is designed to prove the correctness of a particular state transition that emulates a MIPS32 machine.
Cannon does not guarantee that the MIPS32 instructions correctly implement the program that the user intends to prove.
As a result, Cannon's use as a Fault Proof system inherently depends to some extent on the correctness of the compiler
used to generate the MIPS32 instructions over which Cannon operates.

To illustrate this concept, suppose that a user intends to prove simple program `input + 1 = output`.
Suppose then that the user's compiler for this program contains a bug and errantly generates the MIPS instructions for a
slightly different program `input + 2 = output`. Although Cannon would correctly prove the operation of this compiled program,
the result proven would differ from the user's intent. Cannon proves the MIPS state transition but makes no assertion about
the correctness of the translation between the user's high-level code and the resulting MIPS program.

As a consequence of the above, it is the responsibility of a program developer to develop tests that demonstrate that Cannon
is capable of proving their intended program correctly over a large number of possible inputs. Such tests defend against
bugs in the user's compiler as well as ways in which the compiler may inadvertently break one of Cannon's
[Compiler Assumptions](#compiler-assumptions). Users of Fault Proof systems are strongly encouraged to utilize multiple
proof systems and/or compilers to mitigate the impact of errant behavior in any one toolchain.

### Compiler Assumptions

Cannon makes the simplifying assumption that users are utilizing compilers that do not rely on MIPS exception states for
standard program behavior. In other words, Cannon generally assumes that the user's compiler generates spec-compliant
instructions that would not trigger an exception. Refer to [Exceptions](#exceptions) for a list of conditions that are
explicitly handled.

Certain cases that would typically be asserted by a strict implementation of the MIPS32 specification are not handled by
Cannon as follows:

- `add`, `addi`, and `sub` do not trigger an exception on signed integer overflow.
- Instruction encoding validation does not trigger an exception for fields that should be zero.
- Memory instructions do not trigger an exception when addresses are not naturally aligned.

Many compilers, including the Golang compiler, will not generate code that would trigger these conditions under bug-free
operation. Given the inherent reliance on [Compiler Correctness](#compiler-correctness) in applications using Cannon, the
tests and defense mechanisms that must necessarily be employed by Cannon users to protect their particular programs
against compiler bugs should also suffice to surface bugs that would break these compiler assumptions. Stated simply, Cannon
can rely on specific compiler behaviors because users inherently must employ safety nets to guard against compiler bugs.
