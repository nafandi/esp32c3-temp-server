// esp32c3 temperature meter server by nafandi
// Big thanks to impl Ferris (https://github.com/ImplFerris/esp32-projects)
// many of code I just simply copy paste from impl Ferris because I had headache and GERD
#![no_std]
#![no_main]
#![feature(impl_trait_in_assoc_type)]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use core::net::Ipv4Addr;
use core::str::FromStr;
use embassy_executor::Spawner;
use embassy_net::{Ipv4Cidr, StaticConfigV4};
use embassy_net::{Runner, Stack, StackResources};
use embassy_time::{Duration, Timer};
use embedded_dht_rs::dht11::Dht11;
use esp_hal::clock::CpuClock;
use esp_hal::delay::Delay;
use esp_hal::gpio::{DriveMode, Flex, OutputConfig, Pull};
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{
    ClientConfig, ModeConfig, ScanConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
};
use esp32c3_temp_server::dht::{DHT_CACHE, read_dht};
use picoserve::response::IntoResponse;
use picoserve::{AppBuilder, AppRouter, Router, routing, routing::get};

//don't forget to change detail here before using this software
const SSID: &str = "yourwifi";
const PASSWORD: &str = "yourpassword";
const STATIC_IP: &str = "192.168.0.25/24";
const GATEWAY_IP: &str = "192.168.0.1";

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more printlnrmation see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]

pub struct WebApp {
    pub router: &'static Router<<Application as AppBuilder>::PathRouter>,
    pub config: &'static picoserve::Config<Duration>,
}
pub struct Application;

impl AppBuilder for Application {
    type PathRouter = impl routing::PathRouter;

    fn build_app(self) -> picoserve::Router<Self::PathRouter> {
        picoserve::Router::new().route("/", get(sensor_fetch))
    }
}
#[derive(serde::Serialize)]
struct Sensor {
    humidity: u8,
    temperature: u8,
}

pub const WEB_TASK_POOL_SIZE: usize = 2;

impl Default for WebApp {
    fn default() -> Self {
        let router = picoserve::make_static!(AppRouter<Application>, Application.build_app());

        let config = picoserve::make_static!(
            picoserve::Config<Duration>,
            picoserve::Config::new(picoserve::Timeouts {
                start_read_request: Some(Duration::from_secs(5)),
                read_request: Some(Duration::from_secs(1)),
                write: Some(Duration::from_secs(1)),
                persistent_start_read_request: Some(Duration::from_secs(1)),
            })
            .keep_connection_alive()
        );

        Self { router, config }
    }
}
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    println!("esp32c3 - temperature meter server");
    // generator version: 1.2.0

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 66320);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    println!("Embassy initialized!");

    let radio_init = &*mk_static!(
        esp_radio::Controller<'static>,
        esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller")
    );
    let (wifi_controller, interfaces) =
        esp_radio::wifi::new(&radio_init, peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi controller");

    let wifi_interface = interfaces.sta;
    let rng = Rng::new();
    let net_seed = rng.random() as u64 | ((rng.random() as u64) << 32);
    //find the `let net_config` part and replace
    let Ok(ip_addr) = Ipv4Cidr::from_str(STATIC_IP) else {
        println!("Invalid STATIC_IP");
        loop {}
    };

    let Ok(gateway) = Ipv4Addr::from_str(GATEWAY_IP) else {
        println!("Invalid GATEWAY_IP");
        loop {}
    };

    let net_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: ip_addr,
        gateway: Some(gateway),
        dns_servers: Default::default(),
    });
    // You dont need to change anything in `embassy_net::new` call.

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        net_config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        net_seed,
    );
    // init dht11 pin
    let mut dht11_pin = Flex::new(peripherals.GPIO4);
    dht11_pin.apply_output_config(
        &OutputConfig::default()
            .with_drive_mode(DriveMode::OpenDrain)
            .with_pull(Pull::None),
    );
    dht11_pin.set_output_enable(true);
    dht11_pin.set_input_enable(true);
    dht11_pin.set_high();
    let dht11 = Dht11::new(dht11_pin, Delay::new());
    // spawning dht11 reader, wifi connection, and net_task
    spawner.must_spawn(read_dht(dht11));
    spawner.spawn(connection(wifi_controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    wait_for_connection(stack).await;
    println!("enjoy your home temperature check");
    let web_app = WebApp::default();
    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.must_spawn(web_task(id, stack, web_app.router, web_app.config));
    }
    loop {
        Timer::after(Duration::from_secs(1)).await;
    }

    // for inspiration have a look at the examples at https://github.com/esp-rs/esp-hal/tree/esp-hal-v1.0.0/examples
}
// wifi stack
#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        match esp_radio::wifi::sta_state() {
            WifiStaState::Connected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(
                ClientConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            controller.set_config(&client_config).unwrap();
            println!("Starting wifi");
            controller.start_async().await.unwrap();
            println!("Wifi started!");

            println!("Scan");
            let scan_config = ScanConfig::default().with_max(10);
            let result = controller
                .scan_with_config_async(scan_config)
                .await
                .unwrap();
            for ap in result {
                println!("{:?}", ap);
            }
        }
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}
async fn wait_for_connection(stack: Stack<'_>) {
    println!("Waiting for link to be up");
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}
#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

// webserver with picoserve
#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
pub async fn web_task(
    task_id: usize,
    stack: Stack<'static>,
    router: &'static AppRouter<Application>,
    config: &'static picoserve::Config<Duration>,
) -> ! {
    let port = 80;
    let mut tcp_rx_buffer = [0; 1024];
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    picoserve::Server::new(router, config, &mut http_buffer)
        .listen_and_serve(task_id, stack, port, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
        .await
        .into_never()
}

pub async fn sensor_fetch() -> impl IntoResponse {
    let value = {
        let data = DHT_CACHE.lock().await;
        *data
    };

    if let Some(v) = value {
        picoserve::response::Json(Sensor {
            humidity: v.humidity,
            temperature: v.temperature,
        })
    } else {
        picoserve::response::Json(Sensor {
            humidity: 0,
            temperature: 0,
        })
    }
}
