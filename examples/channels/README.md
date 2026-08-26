# Remote channels counting example

This example implements counting over remote channels.
The client asks the server to count up to a number and sends the channel to
count into along with the request. The server counts into that channel and the
client prints each number as it arrives.

The client creates the channel after establishing the TCP connection. Its
sender is transferred to the server as part of the request, while the client
retains the receiver. Remoc carries the channel over the existing connection.

It is split into three crates:

  * `counting` provides the request type shared between client and server.
  * `counting-server` counts into the provided channel and accepts connections
    over TCP.
  * `counting-client` asks the server to count and prints the sequence.

For the same example built on a trait instead of plain channels, see
[the RTC example](../rtc).

## Running

Start the server using the following command:

    cargo run --manifest-path examples/channels/Cargo.toml -p counting-server

Then, in another terminal, start the client using the following command:

    cargo run --manifest-path examples/channels/Cargo.toml -p counting-client

All commands assume that you are in the top-level repository directory.
