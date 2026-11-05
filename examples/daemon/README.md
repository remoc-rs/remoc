# Local timer daemon

A small foreground daemon schedules reminders and notifies clients when they
finish. Timers belong to the daemon: by default, the command that adds one exits
immediately, and the timer keeps running.

The same RPC interface works over Unix domain sockets on Unix and local named
pipes on Windows. There is no TCP port, background service installation or
database. Stopping the daemon discards all timers.

## Running

All commands below assume the repository root as the working directory.

Build both programs:

```sh
cargo build --manifest-path examples/daemon/Cargo.toml --bins
```

In the first terminal, start the daemon:

```sh
cargo run --manifest-path examples/daemon/Cargo.toml --bin timerd
```

In a second terminal, start a watcher:

```sh
cargo run --manifest-path examples/daemon/Cargo.toml --bin timerctl -- watch
```

In a third terminal, add a reminder and list pending timers:

```sh
cargo run --manifest-path examples/daemon/Cargo.toml --bin timerctl -- add 30s "Check tea"
cargo run --manifest-path examples/daemon/Cargo.toml --bin timerctl -- list
```

The add command prints an identifier and exits. After 30 seconds the watcher
prints, for example:

```text
Finished 1: Check tea
```

Run another watcher to see the same notifications in two clients. Each watcher
receives only future completions, not a history. A watcher that cannot keep up
reports a broadcast-channel error rather than slowing down the timers.

To wait for one specific timer instead, add `--wait`:

```sh
cargo run --manifest-path examples/daemon/Cargo.toml --bin timerctl -- add 5s "Check tea" --wait
```

This command prints the identifier, stays connected, then prints the completion.
Watchers also receive that completion. Disconnecting the waiting client does not
cancel its timer.

Use Ctrl+C to stop the daemon, a watcher or a waiting client. On Unix, normal
daemon shutdown removes its socket file. If the daemon was forcibly killed,
first ensure it has stopped, then remove the leftover socket before restarting.

### Choosing an endpoint

On Unix the default is `timerd.sock`, relative to the working directory; run all
programs from the same directory. The socket permits access only by its owner.
On Windows the default is `\\.\pipe\remoc-timerd`.

Both programs accept `--endpoint` before the command. For example, on Unix:

```sh
cargo run --manifest-path examples/daemon/Cargo.toml --bin timerd -- --endpoint /tmp/my-timerd.sock
cargo run --manifest-path examples/daemon/Cargo.toml --bin timerctl -- --endpoint /tmp/my-timerd.sock list
```

Use an application-owned directory for real applications. An existing endpoint
is not removed or taken over when a second daemon starts.

## Code layout

This is one package with a shared library and two binaries:

- [`src/lib.rs`](src/lib.rs) defines the three RPC operations: add, list and
  subscribe. Subscribe returns a Remoc broadcast receiver.
- [`src/bin/timerd.rs`](src/bin/timerd.rs) owns the timers and serves the same
  shared object to every client. Timer tasks are independent of client connections.
- [`src/bin/timerctl.rs`](src/bin/timerctl.rs) makes ordinary calls and receives
  notifications over the connection it opened.
- [`src/unix.rs`](src/unix.rs) and [`src/windows.rs`](src/windows.rs) handle only
  local connection setup. The timer logic and Remoc connection setup do not change
  between platforms.

The `--wait` path shows that a serializable struct can carry a live channel:

1. The CLI creates a Remoc oneshot channel and puts its sender in `NewTimer`.
2. The daemon keeps the sender until the timer finishes, then sends the completed
   `Timer` through it.
3. The CLI awaits the receiver over the same connection.

Without `--wait`, `NewTimer::notify` is `None`.

For a first introduction to remote trait calls, see the simpler
[counter example](../rtc). The [transport snippets](../../remoc/src/connect/transports)
show the connection patterns separately.

