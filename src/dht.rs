// dht.rs by nafandi
// the implementation somehow looks weird because this sh*t assited by GPT-5.3 and Gemini 3
// vibecoding goes brrrt, shrug LOL
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embedded_dht_rs::dht11::Dht11;
use esp_hal::delay::Delay;
use esp_hal::gpio::Flex;
#[derive(Copy, Clone)]
pub struct Dht11Reading {
    pub humidity: u8,
    pub temperature: u8,
}
pub static DHT_CACHE: Mutex<CriticalSectionRawMutex, Option<Dht11Reading>> = Mutex::new(None);

#[embassy_executor::task]
pub async fn read_dht(mut dht: Dht11<Flex<'static>, Delay>) {
    loop {
        match dht.read() {
            Ok(sensor) => {
                let data = Dht11Reading {
                    humidity: sensor.humidity,
                    temperature: sensor.temperature,
                };
                let mut guard = DHT_CACHE.lock().await;
                *guard = Some(data);
            }
            Err(_) => (),
        };
        embassy_time::Timer::after_secs(5).await;
    }
}
