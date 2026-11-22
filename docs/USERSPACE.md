# Native Userspace (Silver Native)

> Status: IMPLEMENTED for everything listed; the crate is `userspace/` and
> builds as separate EL0 tasks.

The native userspace is WerewolfOS's own world: it talks to the kernel
through the native syscall/IPC ABI, has no libc dependency on any Linux
distribution, and keeps working when the Linux ABI is disabled.

## Components

| Component | Role | Status |
|---|---|---|
| `silverd` | Init: spawns the service tree, owns the console | IMPLEMENTED |
| Howl | Shell: scripting basics, pipes/redirection, `linuxrun` to boot Linux stacks | IMPLEMENTED |
| `net_server` | smoltcp userspace stack: DHCP, DNS, TCP, HTTP(S) fetch | IMPLEMENTED |
| Scout (nameserver) | IPC name resolution for native services | IMPLEMENTED |
| Toolbox | ls, cat, echo, md5sum, ping, dns, wget, rm, mv, du, health and more (busybox-style applets linked to one engine) | IMPLEMENTED |
| ELF engine | dynamic ELF loader for native executables | IMPLEMENTED |
| Bundle runtime | manifest + ELF + resources, SHA-256 integrity | IMPLEMENTED |

## ABI surface the native world uses

- Native syscalls: task spawn/exit, IPC channels, capability
  minting/transfer, memory grants, console, timer ticks, SFS through the
  kernel VFS.
- Capability classes for MMIO/IRQ/PCI/console; device drivers run as tasks
  (net_driver + net_server pattern).
- No POSIX: native programs are written against Silver's own ABI, not a
  Linux emulation.

## Howl shell

Console-first shell with:
- job control basics, pipes and redirection,
- the Unix toolbox applets,
- `linuxrun <path>` — boots a Linux personality task (this is how the
  Mobile configuration's OpenRC chain is started: `linuxrun /sfs/sbin/init`).

## What is explicitly NOT here

- No package manager yet (image staging is done host-side by the builders
  in `tools/`; an in-OS manager is PLANNED).
- No graphical shell in the native configuration (graphics belongs to the
  Mobile configuration's Denial WM).
- No daemon supervision framework beyond silverd's fixed tree; a service
  manager (rc.d-like, see DESIGN_DECISIONS.md) is PLANNED.
