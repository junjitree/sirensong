# StarBypass

StarBypass automates the process of connecting to Starbucks Wi-Fi captive
portals. It uses Selenium to simulate a user clicking "Accept" on the terms and
conditions page.

Two versions are available: a Python script and a Rust application.

## Prerequisites

Both versions require:

- `nmcli` (NetworkManager command-line tool)
- Google Chrome
- ChromeDriver (installed and in your `PATH`)

### Python Version

- Python 3
- `selenium` library

### Rust Version

- Rust and Cargo

## Installation

1. Clone this repository:

   ```bash
   git clone https://github.com/junjitree/starbypass.git
   cd starbypass
   ```

### Python Setup

Install the required Python libraries:

```bash
pip install selenium
```

### Rust Setup

The Rust version can be built using Cargo:

```bash
cargo build --release
```

Alternatively, you can install it directly to your system's Cargo bin directory:

```bash
cargo install --path .
```

## Usage

### Python

Run the script:

```bash
python bypass.py [SSID]
```

By default, the script will attempt to connect to the "Starbucks Customer" Wi-Fi
network. You can specify a different SSID as an argument:

```bash
python bypass.py "My Custom SSID"
```

### Rust

If you installed the binary using `cargo install`, you can run it directly:

```bash
starbypass [SSID]
```

Otherwise, run the application using Cargo:

```bash
cargo run -- [SSID]
```

Or run the compiled binary:

```bash
./target/release/starbypass [SSID]
```

Like the Python version, it defaults to "Starbucks Customer" if no SSID is
provided.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file
for details.
