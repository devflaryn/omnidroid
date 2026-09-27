# fork, execve and wait: the design

The boot needs a daemon to start a program: vold's `init_user0` fork-execs
`/system/bin/vold_prepare_subdirs` (`ForkExecvp`), and system_server's SettingsProvider dies without
the `/data/user_de/0` it makes. installd's dexopt later fork-execs `dex2oat`. The ABI layer design
(A) left `fork`, `vfork` and `execve` as `ENOSYS`; this adds them for the shape every one of these
callers has: **fork, a few calls in the child, `execve` or `_exit`**.

## The constraint

A guest address is a host address (D4), and every process of an instance lives in one host
process. A forked child cannot have a copy of its parent's memory at the same addresses. So the
child is a `vfork` child: it runs in its parent's memory until it executes a program or exits,
and the calling thread of the parent waits until then.

A `fork` caller does not follow `vfork`'s rule (the child must not return from the calling
function): bionic's `fork()` returns in the child and `ForkExecvp` goes on to call `execvp`, which
reuses the stack the parent's frames were on. So the kernel keeps what fork promises the parent
for the memory the child can reach that way:

- **the calling thread's stack**, from the parent's `sp` at the `svc` to the top of its mapping,
  is saved at the fork and put back before the parent resumes;
- **the calling thread's `pthread_internal_t` words** the child writes (`CLONE_CHILD_SETTID`
  writes the child's tid into `self->tid`; bionic then caches the pid beside it): the 256 bytes
  from the `child_tid` address are saved and put back.

What else the child writes (the heap) stays written, as under `vfork`. Between `fork` and `execve`
that is at most a few allocations, which the parent never frees; this is the known cost.

## The pieces

1. **`clone` without `CLONE_THREAD`** (`fork`, and `vfork` = `CLONE_VM | CLONE_VFORK`): a child
   `Process` with its own pid, a copy of the fd table (the same open files, `FD_CLOEXEC` kept),
   the cwd, uid, signal actions and umask, and the *parent's* guest space and layout lock. It runs
   on a new host thread from the parent's registers with `x0 = 0`. The parent thread saves the
   memory above, waits for the child's release, restores it, and returns the child's pid.
2. **`execve`** in a vfork child: the program is loaded into a fresh guest space as `spawn` loads
   one, under the child's pid, with the child's fd table less its `FD_CLOEXEC` descriptors, its
   cwd and uid, and the given `argv`/`envp`. The parent is released; the old child image ends
   without a status. `execve` in a process that is not a vfork child stays refused (by name) --
   no caller needs it yet.
3. **Exit**: a child's end (before `execve`, or of the program it executed) releases the parent if
   it still waits, is kept as a zombie in the parent's children, and posts `SIGCHLD` to the
   parent. With `SIGCHLD` ignored (`SIG_IGN` or `SA_NOCLDWAIT`) no zombie is kept.
4. **`wait4`/`waitid`**: a child by pid, any child (`-1`), `WNOHANG`; the Linux status word
   (`code << 8`, or the signal). No children is `ECHILD`. A wait is interrupted by a signal.
5. **`getppid`** answers the parent's pid for a child; `kill` of a child's pid posts to it.

## Out of this design

- A forked child that runs ART (the zygote's shape; `dex2oat` executed by installd) must be its
  own host process (decision 3 of the binder and app boot design). That is the app launch work
  (C5), not this.
- `execve` replacing a process that was not forked.
- Process groups and sessions beyond answering `setsid`/`setpgid`.

## Gate

`tests/fork_exec.rs`, with an NDK fixture `forkexec`:

- fork; in the child `dup2` a pipe onto stdout and `execve("/system/bin/echo", "hello")`; the
  parent reads `hello` from the pipe and `waitpid` answers `WIFEXITED`, status 0;
- the parent's locals and `getpid()` are unchanged after the child ran;
- a child that `_exit(3)`s before exec: `waitpid` answers 3; `WNOHANG` with no ended child
  answers 0; `waitpid` with no children answers `ECHILD`.

And the boot probe: vold's `init_user0` answers success and `/data/user_de/0` exists.
