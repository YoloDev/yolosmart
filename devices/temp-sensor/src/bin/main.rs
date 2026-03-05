#![no_std]
#![no_main]
#![deny(
	clippy::mem_forget,
	reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

extern crate alloc;

use core::pin::pin;

use esp_backtrace as _;
use rs_matter_embassy::matter::dm::devices::DEV_TYPE_ON_OFF_LIGHT;
use tinyrlibc as _;

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::{Duration, Timer};

use esp_alloc::heap_allocator;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::ram;
use esp_hal::timer::timg::TimerGroup;
use esp_metadata_generated::memory_range;
use esp_radio;

use log::info;

use rs_matter_embassy::epoch::epoch;
use rs_matter_embassy::matter::crypto::{Crypto, RngCore, default_crypto};
use rs_matter_embassy::matter::dm::Cluster;
use rs_matter_embassy::matter::dm::clusters;
use rs_matter_embassy::matter::dm::clusters::basic_info::BasicInfoConfig;
use rs_matter_embassy::matter::dm::clusters::decl;
use rs_matter_embassy::matter::dm::clusters::desc::ClusterHandler;
use rs_matter_embassy::matter::dm::clusters::on_off::{OnOffHandler, OnOffHooks, StartUpOnOffEnum};
use rs_matter_embassy::matter::dm::devices::test::{
	DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET,
};
use rs_matter_embassy::matter::dm::{Async, Dataver, EmptyHandler, Endpoint, EpClMatcher, Node};
use rs_matter_embassy::matter::tlv;
use rs_matter_embassy::matter::utils::init::InitMaybeUninit;
use rs_matter_embassy::matter::with;
use rs_matter_embassy::matter::{BasicCommData, clusters, devices};
use rs_matter_embassy::stack::persist::DummyKvBlobStore;
use rs_matter_embassy::stack::rand::reseeding_csprng;
use rs_matter_embassy::wireless::esp::EspThreadDriver;
use rs_matter_embassy::wireless::{EmbassyThread, EmbassyThreadMatterStack};

macro_rules! mk_static {
	($t:ty) => {{
		#[cfg(not(feature = "esp32"))]
		{
			static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
			STATIC_CELL.uninit()
		}
		#[cfg(feature = "esp32")]
		alloc::boxed::Box::leak(alloc::boxed::Box::<$t>::new_uninit())
	}};
}

/// The amount of memory for allocating all `rs-matter-stack` futures created during
/// the execution of the `run*` methods.
/// This does NOT include the rest of the Matter stack.
///
/// The futures of `rs-matter-stack` created during the execution of the `run*` methods
/// are allocated in a special way using a small bump allocator which results
/// in a much lower memory usage by those.
///
/// If - for your platform - this size is not enough, increase it until
/// the program runs without panics during the stack initialization.
const BUMP_SIZE: usize = 18500;

/// Heap strictly necessary only for Wifi+BLE and for the only Matter dependency which needs (~4KB) alloc - `x509`
#[cfg(not(feature = "esp32"))]
const HEAP_SIZE: usize = 100 * 1024;
/// On the esp32, we allocate the Matter Stack from heap as well, due to the non-contiguous memory regions on that chip
#[cfg(feature = "esp32")]
const HEAP_SIZE: usize = 140 * 1024;

const RECLAIMED_RAM: usize =
	memory_range!("DRAM2_UNINIT").end - memory_range!("DRAM2_UNINIT").start;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
	clippy::large_stack_frames,
	reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
	esp_println::logger::init_logger(log::LevelFilter::Info);

	info!("Starting...");

	heap_allocator!(size: HEAP_SIZE - RECLAIMED_RAM);
	heap_allocator!(#[ram(reclaimed)] size: RECLAIMED_RAM);

	// Initialize esp-hal
	let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
	let peripherals = esp_hal::init(config);

	let timg0 = TimerGroup::new(peripherals.TIMG0);
	let sw_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
	esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

	// Create the crypto provider, using the `esp-hal` TRNG/ADC1 as the source of randomness for a reseeding CSPRNG.
	let _trng_source = esp_hal::rng::TrngSource::new(peripherals.RNG, peripherals.ADC1);
	let crypto = default_crypto::<NoopRawMutex, _>(
		reseeding_csprng(esp_hal::rng::Trng::try_new().unwrap(), 1000).unwrap(),
		DAC_PRIVKEY,
	);
	let mut weak_rand = crypto.weak_rand().unwrap();

	// in case there are left-overs from our previous registrations in Thread SRP
	let discriminator = (weak_rand.next_u32() & 0xfff) as u16;

	// TODO: figure out what to do here?
	let mut ieee_eui64 = [0; 8];
	weak_rand.fill_bytes(&mut ieee_eui64);

	// Allocate the Matter stack.
	// For MCUs, it is best to allocate it statically, so as to avoid program stack blowups (its memory footprint is ~ 35 to 50KB).
	// It is also (currently) a mandatory requirement when the wireless stack variation is used.
	let stack = mk_static!(EmbassyThreadMatterStack::<BUMP_SIZE, ()>).init_with(
		EmbassyThreadMatterStack::init(
			&TEST_BASIC_INFO,
			BasicCommData {
				discriminator,
				..TEST_DEV_COMM
			},
			&TEST_DEV_ATT,
			epoch,
		),
	);

	// Light on-off cluster.
	let led = Output::new(peripherals.GPIO15, Level::High, OutputConfig::default());
	let on_off = OnOffHandler::new_standalone(
		Dataver::new_rand(&mut weak_rand),
		LIGHT_ENDPOINT_ID,
		GPIOOnOffDeviceLogic::new(led),
	);

	// Chain out endpoint clusters
	let handler = EmptyHandler
		// Our on-off cluster, on Endpoint 1
		.chain(
			EpClMatcher::new(
				Some(LIGHT_ENDPOINT_ID),
				Some(GPIOOnOffDeviceLogic::CLUSTER.id),
			),
			clusters::on_off::HandlerAsyncAdaptor(&on_off),
		)
		// Each Endpoint needs a Descriptor cluster too
		// Just use the one that `rs-matter` provides out of the box
		.chain(
			EpClMatcher::new(
				Some(LIGHT_ENDPOINT_ID),
				Some(clusters::desc::DescHandler::CLUSTER.id),
			),
			Async(clusters::desc::DescHandler::new(Dataver::new_rand(&mut weak_rand)).adapt()),
		);

	// Create the persister & load any previously saved state
	// `EmbassyPersist`+`EmbassyKvBlobStore` saves to a user-supplied NOR Flash region
	// However, for this demo and for simplicity, we use a dummy persister that does nothing
	let persist = stack
		.create_persist_with_comm_window(&crypto, DummyKvBlobStore)
		.await
		.unwrap();

	// Run the Matter stack with our handler
	// Using `pin!` is completely optional, but reduces the size of the final future
	//
	// This step can be repeated in that the stack can be stopped and started multiple times, as needed.
	let matter = pin!(stack.run_coex(
		// The Matter stack needs to instantiate an `openthread` Radio
		EmbassyThread::new(
			EspThreadDriver::new(peripherals.IEEE802154, peripherals.BT),
			crypto.rand().unwrap(),
			ieee_eui64,
			persist.store(),
			stack,
			true, // Use a random BLE address
		),
		// The Matter stack needs a persister to store its state
		&persist,
		// The crypto provider
		&crypto,
		// Our `AsyncHandler` + `AsyncMetadata` impl
		(NODE, handler),
		// No user future to run
		(),
	));

	// Run Matter
	matter.await.unwrap();
	loop {}
}

// #[embassy_executor::task]
// async fn toggle(mut led: Output<'static>) {
// 	loop {
// 		info!("toggle loop!");
// 		led.toggle();
// 		Timer::after_secs(2).await;
// 	}
// }

struct GPIOOnOffDeviceLogic {
	io: blocking_mutex::Mutex<NoopRawMutex, Output<'static>>,
}

impl GPIOOnOffDeviceLogic {
	pub fn new(io: Output<'static>) -> Self {
		Self {
			io: blocking_mutex::Mutex::new(io),
		}
	}
}

impl OnOffHooks for GPIOOnOffDeviceLogic {
	const CLUSTER: Cluster<'static> = decl::on_off::FULL_CLUSTER
		.with_revision(6)
		.with_attrs(with!(
			required;
			decl::on_off::AttributeId::OnOff
		))
		.with_cmds(with!(
			decl::on_off::CommandId::Off | decl::on_off::CommandId::On | decl::on_off::CommandId::Toggle
		));

	fn on_off(&self) -> bool {
		self.io.borrow().is_set_high()
	}

	fn set_on_off(&self, on: bool) {
		// SAFETY: Not reentrant
		unsafe {
			self
				.io
				.lock_mut(|io| io.set_level(if on { Level::High } else { Level::Low }));
		}
	}

	fn start_up_on_off(&self) -> tlv::Nullable<StartUpOnOffEnum> {
		tlv::Nullable::none()
	}

	fn set_start_up_on_off(
		&self,
		value: tlv::Nullable<StartUpOnOffEnum>,
	) -> Result<(), rs_matter_embassy::matter::error::Error> {
		Ok(())
	}

	async fn handle_off_with_effect(
		&self,
		effect: rs_matter_embassy::matter::dm::clusters::on_off::EffectVariantEnum,
	) {
		// do nothing
	}
}

/// Endpoint 0 (the root endpoint) always runs
/// the hidden Matter system clusters, so we pick ID=1
const LIGHT_ENDPOINT_ID: u16 = 1;

/// Basic info about our device
/// Both the matter stack as well as out mDNS-to-SRP bridge need this, hence extracted out
const TEST_BASIC_INFO: BasicInfoConfig = BasicInfoConfig {
	sai: Some(500),
	..TEST_DEV_DET
};

/// The Matter Light device Node
const NODE: Node = Node {
	id: 0,
	endpoints: &[
		EmbassyThreadMatterStack::<0, ()>::root_endpoint(),
		Endpoint {
			id: LIGHT_ENDPOINT_ID,
			device_types: devices!(DEV_TYPE_ON_OFF_LIGHT),
			clusters: clusters!(
				clusters::desc::DescHandler::CLUSTER,
				GPIOOnOffDeviceLogic::CLUSTER
			),
		},
	],
};
