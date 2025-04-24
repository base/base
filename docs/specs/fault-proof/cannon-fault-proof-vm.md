# Multithreaded Cannon Fault Proof Virtual Machine

<!-- START doctoc generated TOC please keep comment here to allow auto update -->
<!-- DON'T EDIT THIS SECTION, INSTEAD RE-RUN doctoc TO UPDATE -->
**Table of Contents**

- [Overview](#overview)
  - [Definitions](#definitions)
    - [Concepts](#concepts)
      - [Natural Alignment](#natural-alignment)
    - [Data types](#data-types)
    - [Constants](#constants)
  - [New Features](#new-features)
    - [Multithreading](#multithreading)
    - [64-bit Architecture](#64-bit-architecture)
    - [Robustness](#robustness)
- [Multithreading](#multithreading-1)
  - [Thread Management](#thread-management)
  - [Thread Traversal Mechanics](#thread-traversal-mechanics)
    - [Thread Preemption](#thread-preemption)
  - [Exited Threads](#exited-threads)
  - [Futex Operations](#futex-operations)
    - [Wait](#wait)
    - [Wake](#wake)
  - [Voluntary Preemption](#voluntary-preemption)
  - [Forced Preemption](#forced-preemption)
- [Stateful Instructions](#stateful-instructions)
  - [Load Linked / Store Conditional Word](#load-linked--store-conditional-word)
  - [Load Linked / Store Conditional Doubleword](#load-linked--store-conditional-doubleword)
- [FPVM State](#fpvm-state)
  - [State](#state)
  - [State Hash](#state-hash)
  - [Thread State](#thread-state)
  - [Thread Hash](#thread-hash)
  - [Thread Stack Hashing](#thread-stack-hashing)
- [Memory](#memory)
  - [Heap](#heap)
    - [mmap hints](#mmap-hints)
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

The MTCannon FPVM emulates a minimal uniprocessor Linux-based system running on big-endian 64-bit MIPS64 architecture.
A lot of its behaviors are copied from Linux/MIPS with a few tweaks made for fault proofs.
For the rest of this doc, we refer to the MTCannon FPVM as simply the FPVM.

Operationally, the FPVM is a state transition function. This state transition is referred to as a _Step_,
that executes a single instruction. We say the VM is a function $f$, given an input state $S_{pre}$, steps on a
single instruction encoded in the state to produce a new state $S_{post}$.
$$f(S_{pre}) \rightarrow S_{post}$$

Thus, the trace of a program executed by the FPVM is an ordered set of VM states.

### Definitions

#### Concepts

##### Natural Alignment

A memory address is said to be "naturally aligned" in the context of some data type
if it is a multiple of that data type's byte size.
For example, the address of a 32-bit (4-byte) value is naturally aligned if it is a multiple of 4 (e.g. `0x1000`, `0x1004`).
Similarly, the address of a 64-bit (8-byte) value is naturally aligned if it is a multiple of 8 (e.g. `0x1000`, `0x1008`).

A non-aligned address can be naturally aligned by dropping the least significant bits of the address:
`aligned = unaligned & ^(byteSize - 1)`.
For example, to align the address `0x1002` targeting a 32-bit value:
`aligned = 0x1002 & ^(0x3) = 0x1000`.

#### Data types

- `Boolean` - An 8-bit boolean value equal to 0 (false) or 1 (true).
- `Hash` - A 256-bit fixed-size value produced by the Keccak-256 cryptographic hash function.
- `UInt8` - An 8-bit unsigned integer value.
- `UInt64` - A 64-bit unsigned integer value.
- `Word` - A 64-bit value.

#### Constants

- `EBADF` - A Linux error number indicating a bad file descriptor: `0x9`.
- `MaxWord` - A `Word` with all bits set to 1: `0xFFFFFFFFFFFFFFFF`.
When interpreted as a signed value, this is equivalent to -1.
- `ProgramBreakAddress` - The fixed memory address for the program break: `Word(0x0000_4000_0000_0000)`.
- `WordSize` - The number of bytes in a `Word` (8).

### New Features

#### Multithreading

MTCannon adds support for [multithreading](https://en.wikipedia.org/wiki/Thread_(computing)).
Thread management and scheduling are typically handled by the
[operating system (OS) kernel](https://en.wikipedia.org/wiki/Kernel_%28operating_system%29):
programs make thread-related requests to the OS kernel via [syscalls](https://en.wikipedia.org/wiki/System_call).
As such, this implementation includes a few new Linux-specific thread-related [syscalls](#syscalls).
Additionally, the [FPVM state](#fpvm-state) has been modified in order to track the set of active threads
and thread-related global state.

#### 64-bit Architecture

MTCannon emulates a MIPS64 machine whereas STCannon emulates a MIPS32 machine.  The transition from MIPS32 to MIPS64
means the address space goes from 32-bit to 64-bit, greatly expanding addressable memory.

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

### Exited Threads

When the VM encounters an active thread that has exited, it is popped from the active thread stack, removing it from
the VM state.

### Futex Operations

The VM supports [futex syscall](https://www.man7.org/linux/man-pages/man2/futex.2.html) operations
`FUTEX_WAIT_PRIVATE` and `FUTEX_WAKE_PRIVATE`.

Futexes are commonly used to implement locks in user space.
In this scenario, a shared 32-bit value (the "futex value") represents the state of a lock.
If a thread cannot acquire the lock, it calls a futex wait, which puts the thread to sleep.
To release the lock, the owning thread updates the futex value and then calls a futex wake
to notify any other waiting threads.

Because wake-ups may be spurious or could be triggered by unrelated operations on the same memory,
waiting threads must always re-check the futex value after waking up to decide if they can proceed.

#### Wait

When a futex wait is successfully executed, the current thread is simply [preempted](#thread-preemption).
This gives other threads a chance to run and potentially change the shared futex value (for example, by releasing a lock).
When the thread is eventually scheduled again, if the futex value has not changed the wakeup will be considered spurious
and the thread will simply call futex wait again.

#### Wake

When a futex wake is executed, the current thread is [preempted](#thread-preemption). This allows the scheduler to move
on to other threads which may potentially be ready to run (for example, because a shared lock was released).

### Voluntary Preemption

In addition to the [futex syscall](#futex-operations), there are a few other syscalls that
will cause a thread to be "voluntarily" preempted: `sched_yield`, `nanosleep`.

### Forced Preemption

To avoid thread starvation (for example where a thread hogs resources by never executing a sleep, yield, wait, etc.),
the FPVM will force a context switch if the active thread has been executing too long.

For each step executed on a particular thread, the state field `stepsSinceLastContextSwitch` is incremented.
When a thread is preempted, `StepsSinceLastContextSwitch` is reset to 0.
If `StepsSinceLastContextSwitch` reaches a maximum value (`SchedQuantum` = 100_000),
the FPVM preempts the active thread.

## Stateful Instructions

### Load Linked / Store Conditional Word

The Load Linked Word (`ll`) and Store Conditional Word (`sc`) instructions provide the low-level
primitives used to implement atomic read-modify-write (RMW) operations.  A typical RMW sequence might play out as
follows:

- `ll` places a "reservation" targeting a 32-bit value in memory and returns the current value at this location.
- Subsequent instructions take this value and perform some operation on it:
  - For example, maybe a counter variable is loaded and then incremented.
- `sc` is called and the modified value overwrites the original value in memory
only if the memory reservation is still intact.

This RMW sequence ensures that if another thread or process modifies a reserved value while
an atomic update is being performed, the reservation will be invalidated and the atomic update will fail.

Prior to MTCannon, we could be assured that no intervening process would modify such a reserved value because
STCannon is singlethreaded.  With the introduction of multithreading, additional fields need to be stored in the
FPVM state to track memory reservations initiated by `ll` operations.

When an `ll` instruction is executed:

- `llReservationStatus` is set to `1`.
- `llAddress` is set to the virtual memory address specified by `ll`.
- `llOwnerThread` is set to the `threadID` of the active thread.

Only a single memory reservation can be active at a given time - a new reservation will clear any previous reservation.

When the VM writes any data to memory, these `ll`-related fields are checked and any existing memory reservation
is cleared if a memory write touches the naturally-aligned `Word` that contains `llAddress`.

When an `sc` instruction is executed, the operation will only succeed if:

- The `llReservationStatus` field is equal to `1`.
- The active thread's `threadID` matches `llOwnerThread`.
- The virtual address specified by `sc` matches `llAddress`.

On success, `sc` stores a value to the specified address after it is naturally aligned,
clears the memory reservation by zeroing out `llReservationStatus`, `llOwnerThread`, and `llAddress`
and returns `1`.

On failure, `sc` returns `0`.

### Load Linked / Store Conditional Doubleword

With the transition to MIPS64, Load Linked Doubleword (`lld`), and Store Conditional Doubleword (`scd`) instructions
are also now supported.
These instructions are similar to `ll` and `sc`, but they operate on 64-bit rather than 32-bit values.

The `lld` instruction functions similarly to `ll`, but the `llReservationStatus` is set to `2`.
The `scd` instruction functions similarly to `sc`, but the `llReservationStatus` must be equal to `2`
for the operation to succeed.  In other words, an `scd` instruction must be preceded by a matching `lld` instruction
just as the `sc` instruction must be preceded by a matching `ll` instruction if the store operation is to succeed.

## FPVM State

### State

The FPVM is a state transition function that operates on a state object consisting of the following fields:

1. `memRoot` - \[`Hash`\] A value representing the merkle root of VM memory.
1. `preimageKey` - \[`Hash`\] The value of the last requested pre-image key.
1. `preimageOffset` - \[`Word`\] The value of the last requested pre-image offset.
1. `heap` - \[`Word`\] The base address of the most recent memory allocation via mmap.
1. `llReservationStatus` - \[`UInt8`\] The current memory reservation status where: `0` means there is no
   reservation, `1` means an `ll`/`sc`-compatible reservation is active,
   and `2` means an `lld`/`scd`-compatible reservation is active.
   Memory is reserved via Load Linked Word (`ll`)  and Load Linked Doubleword (`lld`) instructions.
1. `llAddress` - \[`Word`\] If a memory reservation is active, the value of
   the address specified by the last `ll` or `lld` instruction.
   Otherwise, set to `0`.
1. `llOwnerThread` - \[`Word`\] The id of the thread that initiated the current memory reservation
   or `0` if there is no active reservation.
1. `exitCode` - \[`UInt8`\] The exit code value.
1. `exited` - \[`Boolean`\] Indicates whether the VM has exited.
1. `step` - \[`UInt64`\] A step counter.
1. `stepsSinceLastContextSwitch` - \[`UInt64`\] A step counter that tracks the number of steps executed on the current
   thread since the last [preemption](#thread-preemption).
1. `traverseRight` - \[`Boolean`\] Indicates whether the currently active thread is on the left or right thread
    stack, as well as some details on thread traversal mechanics.
    See ["Thread Traversal Mechanics"](#thread-traversal-mechanics) for details.
1. `leftThreadStack` - \[`Hash`\] A hash of the contents of the left thread stack.
   For details, see the [“Thread Stack Hashing” section.](#thread-stack-hashing)
1. `rightThreadStack` - \[`Hash`\] A hash of the contents of the right thread stack.
   For details, see the [“Thread Stack Hashing” section.](#thread-stack-hashing)
1. `nextThreadID` - \[`Word`\] The value defining the id to assign to the next thread that is created.

The state is represented by packing the above fields, in order, into a 188-byte buffer.

### State Hash

The state hash is computed by hashing the 188-byte state buffer with the Keccak256 hash function
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

1. `threadID` - \[`Word`\] A unique thread identifier.
1. `exitCode` - \[`UInt8`\] The exit code value.
1. `exited` - \[`Boolean`\] Indicates whether the thread has exited.
1. `pc` - \[`Word`\] The program counter.
1. `nextPC` - \[`Word`\] The next program counter. Note that this value may not always be $pc+4$
   when executing a branch/jump delay slot.
1. `lo` - \[`Word`\] The MIPS LO special register.
1. `hi` - \[`Word`\] The MIPS HI special register.
1. `registers` - 32 general-purpose MIPS registers numbered 0 - 31. Each register contains a `Word` value.

A thread is represented by packing the above fields, in order, into a 298-byte buffer.

### Thread Hash

A thread hash is computed by hashing the 298-byte thread state buffer with the Keccak256 hash function.

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
The tree has a fixed-depth of 59 levels, with leaf values of 32 bytes each.
This spans the full 64-bit address space, where each leaf contains the memory at that part of the tree.
The state `memRoot` represents the merkle root of the tree, reflecting the effects of memory writes.
As a result of this memory representation, all memory operations are `WordSize`-byte aligned.
Memory access doesn't require any privileges. An instruction step can access any memory
location as the entire address space is unprotected.

### Heap

FPVM state contains a `heap` that tracks the base address of the most recent memory allocation.
Heap pages are bump allocated at the page boundary, per `mmap` syscall.
mmap-ing is purely to satisfy program runtimes that need the memory-pointer
result of the syscall to locate free memory. The page size is 4096.

The FPVM has a fixed program break at `ProgramBreakAddress`. However, the FPVM is permitted to extend the
heap beyond this limit via mmap syscalls.
For simplicity, there are no memory protections against "heap overruns" against other memory segments.
Such VM steps are still considered valid state transitions.

Specification of memory mappings is outside the scope of this document as it is irrelevant to
the VM state. FPVM implementers may refer to the Linux/MIPS kernel for inspiration.

#### mmap hints

When a process issues an mmap(2) syscall with a non-NULL addr parameter, the FPVM honors this hint as a strict requirement
rather than a suggestion. The VM unconditionally maps memory at exactly the requested address,
creating the mapping without performing address validity checks.

The VM does not validate whether the specified address range overlaps with existing mappings.
As this is a single-process execution environment, collision detection is delegated to userspace.
The calling process must track its own page mappings to avoid mapping conflicts, as the usual
kernel protections against overlapping mappings are not implemented.

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
register (`$v0`) to `MaxWord` and `errno` (`$a3`) is set accordingly.
The VM must not modify any register other than `$v0` and `$a3` during syscall handling.

The following tables summarize supported syscalls and their behaviors.
If an unsupported syscall is encountered, the VM will raise an exception.

### Supported Syscalls

<!-- cspell:disable -->
| \$v0 | system call   | \$a0            | \$a1             | \$a2         | \$a3             | Effect                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
|------|---------------|-----------------|------------------|--------------|------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 5009 | mmap          | uint64 addr     | uint64 len       | 🚫           | 🚫               | Allocates a page from the heap. See [heap](#heap) for details.                                                                                                                                                                                                                                                                                                                                                                                           |
| 5012 | brk           | 🚫              | 🚫               | 🚫           | 🚫               | Returns a fixed address for the program break at `ProgramBreakAddress`                                                                                                                                                                                                                                                                                                                                                                                   |
| 5205 | exit_group    | uint8 exit_code | 🚫               | 🚫           | 🚫               | Sets the exited and exitCode state fields to `true` and `$a0` respectively.                                                                                                                                                                                                                                                                                                                                                                              |
| 5000 | read          | uint64 fd       | char \*buf       | uint64 count | 🚫               | Similar behavior as Linux/MIPS with support for unaligned reads. See [I/O](#io) for more details.                                                                                                                                                                                                                                                                                                                                                        |
| 5001 | write         | uint64 fd       | char \*buf       | uint64 count | 🚫               | Similar behavior as Linux/MIPS with support for unaligned writes. See [I/O](#io) for more details.                                                                                                                                                                                                                                                                                                                                                       |
| 5070 | fcntl         | uint64 fd       | int64 cmd        | 🚫           | 🚫               | Similar behavior as Linux/MIPS. Only the `F_GETFD`(1) and `F_GETFL` (3) cmds are supported. Sets errno to `0x16` for all other commands.                                                                                                                                                                                                                                                                                                                 |
| 5055 | clone         | uint64 flags    | uint64 stack_ptr | 🚫           | 🚫               | Creates a new thread based on the currently active thread's state.  Supports a `flags` argument equal to `0x00050f00`, other values cause the VM to exit with exit_code `VmStatus.PANIC`.                                                                                                                                                                                                                                                                |
| 5058 | exit          | uint8 exit_code | 🚫               | 🚫           | 🚫               | Sets the active thread's exited and exitCode state fields to `true` and `$a0` respectively.                                                                                                                                                                                                                                                                                                                                                              |
| 5023 | sched_yield   | 🚫              | 🚫               | 🚫           | 🚫               | Preempts the active thread and returns 0.                                                                                                                                                                                                                                                                                                                                                                                                                |
| 5178 | gettid        | 🚫              | 🚫               | 🚫           | 🚫               | Returns the active thread's threadID field.                                                                                                                                                                                                                                                                                                                                                                                                              |
| 5194 | futex         | uint64 addr     | uint64 futex_op  | uint64 val   | uint64 \*timeout | Supports `futex_op`'s `FUTEX_WAIT_PRIVATE` (128) and `FUTEX_WAKE_PRIVATE` (129). Other operations set errno to `0x16`.                                                                                                                                                                                                                                                                                                                                   |
| 5002 | open          | 🚫              | 🚫               | 🚫           | 🚫               | Sets errno to `EBADF`.                                                                                                                                                                                                                                                                                                                                                                                                                            |
| 5034 | nanosleep     | 🚫              | 🚫               | 🚫           | 🚫               | Preempts the active thread and returns 0.                                                                                                                                                                                                                                                                                                                                                                                                                |
| 5222 | clock_gettime | uint64 clock_id | uint64 addr      | 🚫           | 🚫               | Supports `clock_id`'s `REALTIME`(0) and `MONOTONIC`(1). For other `clock_id`'s, sets errno to `0x16`.  Calculates a deterministic time value based on the state's `step` field and a constant `HZ` (10,000,000) where `HZ` represents the approximate clock rate (steps / second) of the FPVM:<br/><br/>`seconds = step/HZ`<br/>`nsecs = (step % HZ) * 10^9/HZ`<br/><br/>Seconds are set at memory address `addr` and nsecs are set at `addr + WordSize`. |
| 5038 | getpid        | 🚫              | 🚫               | 🚫           | 🚫               | Returns 0.                                                                                                                                                                                                                                                                                                                                                                                                                                               |
<!-- cspell:enable -->

### Noop Syscalls

<!-- cspell:disable -->
For the following noop syscalls, the VM must do nothing except to zero out the syscall return (`$v0`)
and errno (`$a3`) registers.

| \$v0 | system call        |
|------|--------------------|
| 5011 | munmap             |
| 5196 | sched_get_affinity |
| 5027 | madvise            |
| 5014 | rt_sigprocmask     |
| 5129 | sigaltstack        |
| 5013 | rt_sigaction       |
| 5297 | prlimit64          |
| 5003 | close              |
| 5016 | pread64            |
| 5004 | stat               |
| 5005 | fstat              |
| 5247 | openat             |
| 5087 | readlink           |
| 5257 | readlinkat         |
| 5015 | ioctl              |
| 5285 | epoll_create1      |
| 5287 | pipe2              |
| 5208 | epoll_ctl          |
| 5272 | epoll_pwait        |
| 5313 | getrandom          |
| 5061 | uname              |
| 5100 | getuid             |
| 5102 | getgid             |
| 5026 | mincore            |
| 5225 | tgkill             |
| 5095 | getrlimit          |
| 5008 | lseek              |
| 5036 | setitimer          |
| 5216 | timer_create       |
| 5217 | timer_settime      |
| 5220 | timer_delete       |
<!-- cspell:enable -->

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

All I/O operations are restricted to a maximum of `WordSize` bytes per operation.
Any read or write syscall request exceeding this limit will be truncated to `WordSize` bytes.
Consequently, the return value of read/write syscalls is at most `WordSize` bytes,
indicating the actual number of bytes read/written.

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
A max `WordSize`-byte chunk of the pre-image at the `preimageOffset` is read to the specified address.
Each read operation increases the `preimageOffset` by the number of bytes requested
(truncated to `WordSize` bytes and subject to alignment constraints).

#### Pre-image I/O Alignment

As mentioned earlier in [memory](#memory), all memory operations are `WordSize`-byte aligned.
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
- Invalid thread state: the active thread stack is empty.

VM implementations may raise an exception in other cases that is specific to the implementation.
For example, an on-chain FPVM that relies on pre-supplied merkle proofs for memory access may
raise an exception if the supplied merkle proof does not match the pre-state `memRoot`.

## Security Model

### Compiler Correctness

MTCannon is designed to prove the correctness of a particular state transition that emulates a MIPS64 machine.
MTCannon does not guarantee that the MIPS64 instructions correctly implement the program that the user intends to prove.
As a result, MTCannon's use as a Fault Proof system inherently depends to some extent on the correctness of the compiler
used to generate the MIPS64 instructions over which MTCannon operates.

To illustrate this concept, suppose that a user intends to prove simple program `input + 1 = output`.
Suppose then that the user's compiler for this program contains a bug and errantly generates the MIPS instructions for a
slightly different program `input + 2 = output`. Although MTCannon would correctly prove the operation of this compiled program,
the result proven would differ from the user's intent. MTCannon proves the MIPS state transition but makes no assertion about
the correctness of the translation between the user's high-level code and the resulting MIPS program.

As a consequence of the above, it is the responsibility of a program developer to develop tests that demonstrate that MTCannon
is capable of proving their intended program correctly over a large number of possible inputs. Such tests defend against
bugs in the user's compiler as well as ways in which the compiler may inadvertently break one of MTCannon's
[Compiler Assumptions](#compiler-assumptions). Users of Fault Proof systems are strongly encouraged to utilize multiple
proof systems and/or compilers to mitigate the impact of errant behavior in any one toolchain.

### Compiler Assumptions

MTCannon makes the simplifying assumption that users are utilizing compilers that do not rely on MIPS exception states for
standard program behavior. In other words, MTCannon generally assumes that the user's compiler generates spec-compliant
instructions that would not trigger an exception. Refer to [Exceptions](#exceptions) for a list of conditions that are
explicitly handled.

Certain cases that would typically be asserted by a strict implementation of the MIPS64 specification are not handled by
MTCannon as follows:

- `add`, `addi`, and `sub` do not trigger an exception on signed integer overflow.
- Instruction encoding validation does not trigger an exception for fields that should be zero.
- Memory instructions do not trigger an exception when addresses are not naturally aligned.

Many compilers, including the Golang compiler, will not generate code that would trigger these conditions under bug-free
operation. Given the inherent reliance on [Compiler Correctness](#compiler-correctness) in applications using MTCannon, the
tests and defense mechanisms that must necessarily be employed by MTCannon users to protect their particular programs
against compiler bugs should also suffice to surface bugs that would break these compiler assumptions. Stated simply, MTCannon
can rely on specific compiler behaviors because users inherently must employ safety nets to guard against compiler bugs.
