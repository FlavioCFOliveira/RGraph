# Installation

## Requirements

- **Rust** 1.96.0 or later (stable channel)
- **protoc** (Protocol Buffers compiler) — required to build the gRPC schema
- **Linux** (x86_64 or aarch64) — other platforms are best-effort

## Building from Source

```bash
git clone https://github.com/FlavioCFOliveira/RGraph.git
cd RGraph

# Install the protobuf compiler (Debian/Ubuntu)
sudo apt-get install protobuf-compiler

# Build the release binary
cargo build --release

# The binary is now available at:
#   target/release/rgraph
```

## Verify the Build

```bash
./target/release/rgraph --help
```

You should see the top-level CLI help with subcommands (`init`, `open`,
`insert`, `read`, `serve`, `import`, `export`, `benchmark`).
