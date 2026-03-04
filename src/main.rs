use std::env;
use std::process::{Command, Stdio};
use std::time::Duration;
use thirtyfour::prelude::*;
use tokio::time::sleep;

const GREEN_START: &str = "\x1b[32m";
const GREEN_END: &str = "\x1b[0m";
const RED_START: &str = "\x1b[31m";
const RED_END: &str = "\x1b[0m";

fn error(msg: &str) {
    eprintln!("{}", msg);
}

fn connect_to_wifi(ssid: &str) -> bool {
    let right = format!("{}✅{}", GREEN_START, GREEN_END);
    let left = format!("{}🚫{}", RED_START, RED_END);

    println!("{}Connecting to {}...", right, ssid);

    let nmcli_up = Command::new("nmcli")
        .args(["connection", "up", ssid])
        .output();

    match nmcli_up {
        Ok(output) if output.status.success() => {
            println!("{}Network command sent successfully...", right);
        }
        Ok(_) | Err(_) => {
            error(&format!("{}Failed to connect to WiFi: {}!!!", left, ssid));
            return false;
        }
    }

    let mut tries = 1;
    while tries < 20 {
        let result = Command::new("nmcli")
            .args(["-t", "-f", "active,ssid", "dev", "wifi"])
            .output();

        if let Ok(output) = result
            && output.status.success()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut current_ssid = String::new();
            for line in stdout.lines() {
                if line.starts_with("yes:") {
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.len() > 1 {
                        current_ssid = parts[1].to_string();
                    }
                    break;
                }
            }

            if current_ssid == ssid {
                println!("{}Connected after {} tries...", right, tries);
                return true;
            }
        }

        tries += 1;
        std::thread::sleep(Duration::from_millis(500));
    }

    error(&format!("{}Failed to connect to WiFi: {}!!!", left, ssid));
    false
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    let ssid = if args.len() > 1 {
        args[1].clone()
    } else {
        "Starbucks Customer".to_string()
    };

    let right = format!("{}✅{}", GREEN_START, GREEN_END);
    let left = format!("{}🚫{}", RED_START, RED_END);

    if !connect_to_wifi(&ssid) {
        error(&format!(
            "{}Aborting: Could not establish network connection!!!",
            left
        ));
        std::process::exit(1);
    }

    #[allow(clippy::zombie_processes)]
    let mut chromedriver = Command::new("chromedriver")
        .arg("--port=9515")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to start chromedriver. Make sure it's installed and in your PATH.");

    sleep(Duration::from_secs(1)).await;

    let mut caps = DesiredCapabilities::chrome();
    caps.add_arg("--headless=new")?;
    caps.add_arg("--no-sandbox")?;
    caps.add_arg("--disable-dev-shm-usage")?;

    let driver_result = WebDriver::new("http://localhost:9515", caps).await;
    let driver = match driver_result {
        Ok(driver) => driver,
        Err(e) => {
            let _ = chromedriver.kill();
            error(&format!("{}Failed to connect to webdriver: {}!!!", left, e));
            std::process::exit(1);
        }
    };

    println!("{}Navigating to trigger portal...", right);
    if let Err(e) = driver.goto("http://google.com").await {
        error(&format!("{}Failed to navigate: {}!!!", left, e));
        let _ = driver.quit().await;
        let _ = chromedriver.kill();
        std::process::exit(1);
    }

    println!("{}Checking if we are already authenticated...", right);

    let mut authenticated = false;
    for _ in 0..15 {
        if let Ok(url) = driver.current_url().await
            && (url.as_str().starts_with("https://google.com")
                || url.as_str().starts_with("https://www.google.com"))
        {
            authenticated = true;
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }

    if authenticated {
        println!("{}We're already authenticated...", right);
    } else {
        println!("{}Looking for radio button...", right);

        let mut clicked_radio = false;
        for _ in 0..15 {
            if let Ok(radio) = driver.find(By::Id("option_free")).await
                && radio.is_displayed().await.unwrap_or(false)
                && radio.click().await.is_ok()
            {
                clicked_radio = true;
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }

        if clicked_radio {
            sleep(Duration::from_secs(1)).await;

            println!("{}Looking for submit button...", right);
            let mut submitted = false;
            for _ in 0..15 {
                if let Ok(submit) = driver.find(By::Name("commit")).await
                    && submit.is_displayed().await.unwrap_or(false)
                    && let Ok(_) = submit.click().await
                {
                    submitted = true;
                    break;
                }
                sleep(Duration::from_secs(1)).await;
            }

            if submitted {
                println!("{}Success! Portal submitted...", right);
            } else {
                error(&format!("{}Could not find or click submit button!!!", left));
            }
        } else {
            error(&format!("{}Could not find or click radio button!!!", left));
        }
    }

    let _ = driver.quit().await;
    let _ = chromedriver.kill();

    Ok(())
}
