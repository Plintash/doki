# Spec Delta

## Purpose

Makes terminals daemon-owned so a shell outlives any one window, clients can
attach to a running terminal and replay its recent output, and the desktop
renders and emulates the byte stream it receives without owning the process.

## ADDED Requirements

### Requirement: The daemon owns terminal process lifetime

A terminal SHALL be created in and owned by the daemon. Its shell MUST keep
running when the client that opened it disconnects or its window closes, until
the terminal is explicitly closed or its task is removed.

#### Scenario: Window close keeps the shell

- **WHEN** a terminal is running a long-lived process and the window closes
- **THEN** the process keeps running in the daemon

#### Scenario: Reattach restores the terminal

- **WHEN** a client attaches to a terminal that is still running
- **THEN** it receives the terminal's retained output and can interact with the
  same shell

#### Scenario: Explicit close ends the shell

- **WHEN** the last client closes a terminal
- **THEN** the daemon terminates the shell and releases the terminal

### Requirement: Terminals retain bounded scrollback for replay

The daemon SHALL retain a bounded scrollback buffer per terminal and replay it
when a client attaches, so the client can reconstruct the visible grid and
scrollback without having been present for every byte.

#### Scenario: Attach after absence

- **WHEN** a terminal produced output while no client was attached and a client
  then attaches
- **THEN** the client can reconstruct the recent grid and scrollback from the
  replay

#### Scenario: Buffer stays bounded

- **WHEN** a terminal produces more output than the buffer holds
- **THEN** older output is dropped and daemon memory stays bounded

### Requirement: Terminal rendering stays client-side

Terminal emulation, selection, search, and scrolling SHALL remain in the
client. The daemon MUST stream output bytes and MUST NOT depend on a client to
drive frame-by-frame grid state.

#### Scenario: Two clients attach

- **WHEN** two clients attach to one terminal
- **THEN** both reconstruct the same grid independently and each can scroll or
  select without affecting the other

### Requirement: Output is coalesced under load

Terminal output SHALL be delivered in bounded batches so a fast producer cannot
drive one message per write or grow unbounded memory, and the client MUST stay
responsive while output drains.

#### Scenario: Flood of output

- **WHEN** a command writes megabytes to the terminal
- **THEN** the daemon delivers the output in bounded batches, memory stays
  bounded, and the client remains responsive

### Requirement: Input and resize reach the terminal

A client SHALL be able to write input bytes and resize a terminal it is
attached to, and the resize MUST be reflected by the shell process.

#### Scenario: Resize propagates

- **WHEN** the client resizes the terminal surface
- **THEN** the daemon applies the new size to the shell's terminal

### Requirement: Terminal identity is scoped to its runtime

A terminal SHALL be scoped to the task and runtime that created it. Writes from
a client bound to a superseded runtime MUST be rejected rather than delivered
to the replacement's shell.

#### Scenario: Stale client write

- **WHEN** a task's runtime is replaced while a client still holds the old
  terminal
- **THEN** the daemon rejects that client's input to the old terminal
