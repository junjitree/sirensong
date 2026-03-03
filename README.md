# StarBypass

StarBypass is a Python script that automates the process of connecting to
Starbucks Wi-Fi captive portals. It uses Selenium to simulate a user clicking
"Accept" on the terms and conditions page.

## Prerequisites

- Python 3
- `nmcli` (NetworkManager command-line tool)
- Google Chrome
- ChromeDriver

## Installation

1. Clone this repository:

    ```bash
    git clone https://github.com/junjitree/starbypass.git
    cd starbypass
    ```

2. Install the required Python libraries:

    ```bash
    pip install selenium
    ```

3. Make sure you have Google Chrome and the corresponding ChromeDriver
    installed and in your PATH.

## Usage

Run the script:

```bash
python bypass.py
```

The script will attempt to connect to the "Starbucks Customer" Wi-Fi network and
then automate the captive portal login.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file
for details.
