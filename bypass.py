import sys
import subprocess
import time
from selenium import webdriver
from selenium.webdriver.chrome.options import Options
from selenium.webdriver.common.by import By
from selenium.webdriver.support.ui import WebDriverWait
from selenium.webdriver.support import expected_conditions as EC

green_start = "\033[32m"
green_end = "\033[0m"
red_start = "\033[31m"
red_end = "\033[0m"
right = f"{green_start}✅{green_end}"
left = f"{red_start}🚫{red_end}"
ssid = sys.argv[1] if len(sys.argv) > 1 else "Starbucks Customer"


def error(string):
    print(string, file=sys.stderr)


def connect_to_wifi(ssid):
    print(f"{right}Connecting to {ssid}...")
    try:
        subprocess.run(
            ["nmcli", "connection", "up", ssid], capture_output=True, check=True
        )
        print(f"{right}Network command sent successfully...")
        tries = 1
        while tries < 20:
            result = subprocess.run(
                [
                    "nmcli",
                    "-t",
                    "-f",
                    "active,ssid",
                    "dev",
                    "wifi",
                ],
                capture_output=True,
                text=True,
                check=True,
            )

            current_ssid = ""
            for line in result.stdout.splitlines():
                if line.startswith("yes:"):
                    current_ssid = line.split(":")[1]
                    break

            if current_ssid == ssid:
                print(f"{right}Connected after {tries} tries...")
                return True

            tries += 1
            time.sleep(0.5)

        error(f"{left}Failed to connect to WiFi: {ssid}!!!")
        return False
    except subprocess.CalledProcessError as _:
        error(f"{left}Failed to connect to WiFi: {ssid}!!!")
        return False


if not connect_to_wifi(ssid):
    error(f"{left}Aborting: Could not establish network connection!!!")
    sys.exit(1)

chrome_options = Options()
chrome_options.add_argument("--headless=new")
chrome_options.add_argument("--no-sandbox")
chrome_options.add_argument("--disable-dev-shm-usage")

driver = webdriver.Chrome(options=chrome_options)

try:
    print(f"{right}Navigating to trigger portal...")
    driver.get("http://google.com")

    wait = WebDriverWait(driver, 15)

    print(f"{right}Checking if we are already authenticated...")
    try:
        header = wait.until(EC.url_changes("https://google.com"))
        print(f"{right}We're already authenticated...")
        driver.quit()
    except Exception as _:
        print(f"{right}Looking for radio button...")
        radio_button = wait.until(EC.element_to_be_clickable((By.ID, "option_free")))
        radio_button.click()

        time.sleep(1)

        print(f"{right}Looking for submit button...")
        submit_button = wait.until(EC.element_to_be_clickable((By.NAME, "commit")))
        submit_button.click()

        print(f"{right}Success! Portal submitted...")

except Exception as e:
    error(f"{left}An error occurred: {e}!!!")
finally:
    driver.quit()
