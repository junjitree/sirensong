import subprocess
import time
from selenium import webdriver
from selenium.webdriver.chrome.options import Options
from selenium.webdriver.common.by import By
from selenium.webdriver.support.ui import WebDriverWait
from selenium.webdriver.support import expected_conditions as EC


def connect_to_wifi(ssid):
    print(f"Connecting to {ssid}...")
    try:
        subprocess.run(["nmcli", "connection", "up", ssid], check=True)
        print("Network command sent successfully.")
        time.sleep(5)
        return True
    except subprocess.CalledProcessError as e:
        print(f"Failed to connect to WiFi: {e}")
        return False


ssid_name = "Starbucks Customer"
if connect_to_wifi(ssid_name):

    chrome_options = Options()
    chrome_options.add_argument("--headless=new")
    chrome_options.add_argument("--no-sandbox")
    chrome_options.add_argument("--disable-dev-shm-usage")

    driver = webdriver.Chrome(options=chrome_options)

    try:
        print("Navigating to trigger portal...")
        driver.get("http://neverssl.com")

        wait = WebDriverWait(driver, 15)

        print("Looking for radio button...")
        radio_button = wait.until(EC.element_to_be_clickable((By.ID, "option_free")))
        radio_button.click()

        time.sleep(1)

        print("Looking for submit button...")
        submit_button = wait.until(EC.element_to_be_clickable((By.NAME, "commit")))
        submit_button.click()

        print("Success! Portal submitted.")

    except Exception as e:
        print(f"An error occurred: {e}")
    finally:
        driver.quit()
else:
    print("Aborting: Could not establish network connection.")
