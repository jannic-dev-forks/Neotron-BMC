//! Neotron BMC Firmware
//!
//! This is the firmware for the Neotron Board Management Controller (BMC) as
//! fitted to a Neotron Pico. It controls the power, reset, UART and PS/2 ports
//! on that Neotron mainboard. For more details, see the `README.md` file.
//!
//! # Licence
//! This source code as a whole is licensed under the GPL v3. Third-party crates
//! are covered by their respective licences.

#![no_main]
#![no_std]

use heapless::spsc::{Consumer, Producer, Queue};
use rtic::app;
use stm32f0xx_hal::{
	gpio::gpioa::{PA10, PA11, PA12, PA15, PA2, PA3, PA4, PA9},
	gpio::gpiob::{PB0, PB1, PB3, PB4, PB5},
	gpio::gpiof::{PF0, PF1},
	gpio::{Alternate, Floating, Input, Output, PullUp, PushPull, AF1},
	pac,
	prelude::*,
	serial,
};

use neotron_bmc_pico as _;
use neotron_bmc_protocol as proto;

/// Version string auto-generated by git.
static VERSION: &'static str = include_str!(concat!(env!("OUT_DIR"), "/version.txt"));

/// At what rate do we blink the status LED when we're running?
const LED_PERIOD_MS: u64 = 1000;

/// How often we poll the power and reset buttons in milliseconds.
const DEBOUNCE_POLL_INTERVAL_MS: u64 = 75;

/// Length of a reset pulse, in milliseconds
const RESET_DURATION_MS: u64 = 250;

/// The states we can be in controlling the DC power
#[derive(Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum DcPowerState {
	/// We've just enabled the DC power (so ignore any incoming long presses!)
	Starting = 1,
	/// We are now fully on. Look for a long press to turn off.
	On = 2,
	/// We are fully off.
	Off = 0,
}

/// This is our system state, as accessible via SPI reads and writes.
#[derive(Debug)]
pub struct RegisterState {
	firmware_version: [u8; 32],
}

#[app(device = crate::pac, peripherals = true, dispatchers = [USB, USART3_4_5_6, TIM14, TIM15, TIM16, TIM17, PVD])]
mod app {
	use super::*;
	use systick_monotonic::*; // Implements the `Monotonic` trait

	pub enum Message {
		/// Word from PS/2 port 0
		Ps2Data0(u16),
		/// Word from PS/2 port 1
		Ps2Data1(u16),
		/// Message from SPI bus
		SpiRequest(neotron_bmc_protocol::Request),
		/// The power button was given a tap
		PowerButtonShortPress,
		/// The power button was held down
		PowerButtonLongPress,
		/// The reset button was given a tap
		ResetButtonShortPress,
		/// The UART got some data
		UartByte(u8),
	}

	#[shared]
	struct Shared {
		/// The power LED (D1101)
		#[lock_free]
		led_power: PB0<Output<PushPull>>,
		/// The status LED (D1102)
		#[lock_free]
		_buzzer_pwm: PB1<Output<PushPull>>,
		/// The FTDI UART header (J105)
		#[lock_free]
		serial: serial::Serial<pac::USART1, PA9<Alternate<AF1>>, PA10<Alternate<AF1>>>,
		/// The Clear-To-Send line on the FTDI UART header (which the serial object can't handle)
		#[lock_free]
		_pin_uart_cts: PA11<Alternate<AF1>>,
		/// The Ready-To-Receive line on the FTDI UART header (which the serial object can't handle)
		#[lock_free]
		_pin_uart_rts: PA12<Alternate<AF1>>,
		/// The power button
		#[lock_free]
		button_power: PF0<Input<PullUp>>,
		/// The reset button
		#[lock_free]
		button_reset: PF1<Input<PullUp>>,
		/// Tracks DC power state
		#[lock_free]
		state_dc_power_enabled: DcPowerState,
		/// Controls the DC-DC PSU
		#[lock_free]
		pin_dc_on: PA3<Output<PushPull>>,
		/// Controls the Reset signal across the main board, putting all the
		/// chips (except this BMC!) in reset when pulled low.
		#[lock_free]
		pin_sys_reset: PA2<Output<PushPull>>,
		/// Clock pin for PS/2 Keyboard port
		#[lock_free]
		ps2_clk0: PA15<Input<Floating>>,
		/// Clock pin for PS/2 Mouse port
		#[lock_free]
		_ps2_clk1: PB3<Input<Floating>>,
		/// Data pin for PS/2 Keyboard port
		#[lock_free]
		ps2_dat0: PB4<Input<Floating>>,
		/// Data pin for PS/2 Mouse port
		#[lock_free]
		_ps2_dat1: PB5<Input<Floating>>,
		/// The external interrupt peripheral
		#[lock_free]
		exti: pac::EXTI,
		/// Our register state
		#[lock_free]
		register_state: RegisterState,
		/// Read messages here
		#[lock_free]
		msg_q_out: Consumer<'static, Message, 8>,
		/// Write messages here
		msg_q_in: Producer<'static, Message, 8>,
		/// SPI Peripheral
		spi: neotron_bmc_pico::spi::SpiPeripheral<5, 64>,
		/// CS pin
		pin_cs: PA4<Input<PullUp>>,
	}

	#[local]
	struct Local {
		/// Tracks power button state for short presses. 75ms x 2 = 150ms is a short press
		press_button_power_short: debouncr::Debouncer<u8, debouncr::Repeat2>,
		/// Tracks power button state for long presses. 75ms x 16 = 1200ms is a long press
		press_button_power_long: debouncr::Debouncer<u16, debouncr::Repeat16>,
		/// Tracks reset button state for short presses. 75ms x 2 = 150ms is a long press
		press_button_reset_short: debouncr::Debouncer<u8, debouncr::Repeat2>,
		/// Keyboard PS/2 decoder
		kb_decoder: neotron_bmc_pico::ps2::Ps2Decoder,
	}

	#[monotonic(binds = SysTick, default = true)]
	type MyMono = Systick<200>; // 200 Hz (= 5ms) timer tick

	/// The entry point to our application.
	///
	/// Sets up the hardware and spawns the regular tasks.
	///
	/// * Task `led_power_blink` - blinks the LED
	/// * Task `button_poll` - checks the power and reset buttons
	#[init(local = [ queue: Queue<Message, 8> = Queue::new()])]
	fn init(ctx: init::Context) -> (Shared, Local, init::Monotonics) {
		defmt::info!("Neotron BMC version {:?} booting", VERSION);

		let dp: pac::Peripherals = ctx.device;
		let cp: cortex_m::Peripherals = ctx.core;

		let mut flash = dp.FLASH;
		let mut rcc = dp
			.RCC
			.configure()
			.hclk(48.mhz())
			.pclk(48.mhz())
			.sysclk(48.mhz())
			.freeze(&mut flash);

		defmt::info!("Configuring SysTick...");
		// Initialize the monotonic timer using the Cortex-M SysTick peripheral
		let mono = Systick::new(cp.SYST, rcc.clocks.sysclk().0);

		defmt::info!("Creating pins...");
		let gpioa = dp.GPIOA.split(&mut rcc);
		let gpiob = dp.GPIOB.split(&mut rcc);
		let gpiof = dp.GPIOF.split(&mut rcc);
		// We have to have the closure return a tuple of all our configured
		// pins because by taking fields from `gpioa`, `gpiob`, etc, we leave
		// them as partial structures. This prevents us from having a call to
		// `disable_interrupts` for each pin. We can't simply do the `let foo
		// = ` inside the closure either, as the pins would be dropped when
		// the closure ended. So, we have this slightly awkward syntax
		// instead. Do ensure the pins and the variables line-up correctly;
		// order is important!
		let (
			uart_tx,
			uart_rx,
			_pin_uart_cts,
			_pin_uart_rts,
			mut led_power,
			mut _buzzer_pwm,
			button_power,
			button_reset,
			mut pin_dc_on,
			mut pin_sys_reset,
			ps2_clk0,
			_ps2_clk1,
			ps2_dat0,
			_ps2_dat1,
			pin_cs,
			pin_sck,
			pin_cipo,
			pin_copi,
		) = cortex_m::interrupt::free(|cs| {
			(
				// uart_tx,
				gpioa.pa9.into_alternate_af1(cs),
				// uart_rx,
				gpioa.pa10.into_alternate_af1(cs),
				// _pin_uart_cts,
				gpioa.pa11.into_alternate_af1(cs),
				// _pin_uart_rts,
				gpioa.pa12.into_alternate_af1(cs),
				// led_power,
				gpiob.pb0.into_push_pull_output(cs),
				// _buzzer_pwm,
				gpiob.pb1.into_push_pull_output(cs),
				// button_power,
				gpiof.pf0.into_pull_up_input(cs),
				// button_reset,
				gpiof.pf1.into_pull_up_input(cs),
				// pin_dc_on,
				gpioa.pa3.into_push_pull_output(cs),
				// pin_sys_reset,
				gpioa.pa2.into_push_pull_output(cs),
				// ps2_clk0,
				gpioa.pa15.into_floating_input(cs),
				// _ps2_clk1,
				gpiob.pb3.into_floating_input(cs),
				// ps2_dat0,
				gpiob.pb4.into_floating_input(cs),
				// _ps2_dat1,
				gpiob.pb5.into_floating_input(cs),
				// pin_cs,
				gpioa.pa4.into_pull_up_input(cs),
				// pin_sck,
				gpioa.pa5.into_alternate_af0(cs),
				// pin_cipo,
				{
					// Force 'high speed' mode first, then go into AF0 (high
					// speed mode is sticky in this HAL revision).
					let pin = gpioa.pa6.into_push_pull_output_hs(cs);
					pin.into_alternate_af0(cs)
				},
				// pin_copi,
				gpioa.pa7.into_alternate_af0(cs),
			)
		});

		pin_sys_reset.set_low().unwrap();
		pin_dc_on.set_low().unwrap();

		defmt::info!("Creating UART...");

		let mut serial =
			serial::Serial::usart1(dp.USART1, (uart_tx, uart_rx), 115_200.bps(), &mut rcc);

		serial.listen(serial::Event::Rxne);

		// Put SPI into Peripheral mode (i.e. CLK is an input) and enable the RX interrupt.
		let spi = neotron_bmc_pico::spi::SpiPeripheral::new(
			dp.SPI1,
			(pin_sck, pin_cipo, pin_copi),
			8_000_000,
			&mut rcc,
		);

		led_power.set_low().unwrap();
		_buzzer_pwm.set_low().unwrap();

		// Set EXTI15 to use PORT A (PA15) - button input
		dp.SYSCFG.exticr4.modify(|_r, w| w.exti15().pa15());

		// Enable EXTI15 interrupt as external falling edge
		dp.EXTI.imr.modify(|_r, w| w.mr15().set_bit());
		dp.EXTI.emr.modify(|_r, w| w.mr15().set_bit());
		dp.EXTI.ftsr.modify(|_r, w| w.tr15().set_bit());

		// Set EXTI4 to use PORT A (PA4) - SPI CS
		dp.SYSCFG.exticr2.modify(|_r, w| w.exti4().pa4());

		// Enable EXTI4 interrupt as external falling/rising edge
		dp.EXTI.imr.modify(|_r, w| w.mr4().set_bit());
		dp.EXTI.emr.modify(|_r, w| w.mr4().set_bit());
		dp.EXTI.ftsr.modify(|_r, w| w.tr4().set_bit());
		dp.EXTI.rtsr.modify(|_r, w| w.tr4().set_bit());

		// Spawn the tasks that run all the time
		led_power_blink::spawn().unwrap();
		button_poll::spawn().unwrap();

		defmt::info!("Init complete!");

		let (msg_q_in, msg_q_out) = ctx.local.queue.split();

		let shared_resources = Shared {
			serial,
			_pin_uart_cts,
			_pin_uart_rts,
			led_power,
			_buzzer_pwm,
			button_power,
			button_reset,
			state_dc_power_enabled: DcPowerState::Off,
			pin_dc_on,
			pin_sys_reset,
			ps2_clk0,
			_ps2_clk1,
			ps2_dat0,
			_ps2_dat1,
			exti: dp.EXTI,
			register_state: RegisterState {
				firmware_version:
					*b"Neotron BMC v0.3.1\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
			},
			msg_q_out,
			msg_q_in,
			spi,
			pin_cs,
		};
		let local_resources = Local {
			press_button_power_short: debouncr::debounce_2(false),
			press_button_power_long: debouncr::debounce_16(false),
			press_button_reset_short: debouncr::debounce_2(false),
			kb_decoder: neotron_bmc_pico::ps2::Ps2Decoder::new(),
		};
		let init = init::Monotonics(mono);
		(shared_resources, local_resources, init)
	}

	/// Our idle task.
	///
	/// This task is called when there is nothing else to do.
	#[idle(shared = [msg_q_out, msg_q_in, spi, register_state])]
	fn idle(mut ctx: idle::Context) -> ! {
		defmt::info!("Idle is running...");
		loop {
			match ctx.shared.msg_q_out.dequeue() {
				Some(Message::Ps2Data0(word)) => {
					if let Some(byte) = neotron_bmc_pico::ps2::Ps2Decoder::check_word(word) {
						defmt::info!("< KB 0x{:x}", byte);
					} else {
						defmt::warn!("< Bad KB 0x{:x}", word);
					}
				}
				Some(Message::Ps2Data1(word)) => {
					if let Some(byte) = neotron_bmc_pico::ps2::Ps2Decoder::check_word(word) {
						defmt::info!("< MS 0x{:x}", byte);
					} else {
						defmt::warn!("< Bad MS 0x{:x}", word);
					}
				}
				Some(Message::PowerButtonLongPress) => {}
				Some(Message::PowerButtonShortPress) => {}
				Some(Message::ResetButtonShortPress) => {}
				Some(Message::SpiRequest(req)) => match req.request_type {
					proto::RequestType::Read | proto::RequestType::ReadAlt => {
						let rsp = match req.register {
							0x00 => {
								let length = req.length_or_data as usize;
								if length > ctx.shared.register_state.firmware_version.len() {
									proto::Response::new_without_data(
										proto::ResponseResult::BadLength,
									)
								} else {
									let bytes = &ctx.shared.register_state.firmware_version;
									proto::Response::new_ok_with_data(&bytes[0..length])
								}
							}
							_ => proto::Response::new_without_data(
								proto::ResponseResult::BadRegister,
							),
						};
						ctx.shared.spi.lock(|spi| {
							spi.set_transmit_sendable(&rsp).unwrap();
						});
					}
					_ => {
						let rsp =
							proto::Response::new_without_data(proto::ResponseResult::BadLength);
						ctx.shared.spi.lock(|spi| {
							spi.set_transmit_sendable(&rsp).unwrap();
						});
					}
				},
				Some(Message::UartByte(rx_byte)) => {
					defmt::info!("UART RX {:?}", rx_byte);
					// TODO: Copy byte to software buffer and turn UART RX
					// interrupt off if buffer is full
				}
				None => {
					// No messages
				}
			}

			// Look for something in the SPI bytes received buffer:
			let mut req = None;
			ctx.shared.spi.lock(|spi| {
				let mut mark_done = false;
				if let Some(data) = spi.get_received() {
					use proto::Receivable;
					match proto::Request::from_bytes(data) {
						Ok(inner_req) => {
							mark_done = true;
							req = Some(inner_req);
						}
						Err(proto::Error::BadLength) => {
							// Need more data
						}
						Err(e) => {
							defmt::warn!("Bad Req ({:02x})", e as u8);
							mark_done = true;
						}
					}
				}
				if mark_done {
					// Couldn't do this whilst holding the `data` ref.
					spi.mark_done();
				}
			});

			// If we got a valid message, queue it so we can look at it next time around
			if let Some(req) = req {
				if ctx
					.shared
					.msg_q_in
					.lock(|q| q.enqueue(Message::SpiRequest(req)))
					.is_err()
				{
					panic!("Q full!");
				}
			}
			// TODO: Read ADC for 3.3V and 5.0V rails and check good
		}
	}

	/// This is the external GPIO interrupt task.
	///
	/// It handles PS/2 clock edges, and SPI chip select edges.
	///
	/// It is very high priority, as we can't afford to miss a PS/2 clock edge.
	#[task(
		binds = EXTI4_15,
		priority = 4,
		shared = [ps2_clk0, msg_q_in, ps2_dat0, exti, spi, pin_cs],
		local = [kb_decoder]
	)]
	fn exti4_15_interrupt(mut ctx: exti4_15_interrupt::Context) {
		let pr = ctx.shared.exti.pr.read();
		// Is this EXT15 (PS/2 Port 0 clock input)
		if pr.pr15().bit_is_set() {
			let data_bit = ctx.shared.ps2_dat0.is_high().unwrap();
			// Do we have a complete word?
			if let Some(data) = ctx.local.kb_decoder.add_bit(data_bit) {
				// Don't dump in the ISR - we're busy. Add it to this nice lockless queue instead.
				if ctx
					.shared
					.msg_q_in
					.lock(|q| q.enqueue(Message::Ps2Data0(data)))
					.is_err()
				{
					panic!("queue full");
				};
			}
			// Clear the pending flag for this pin
			ctx.shared.exti.pr.write(|w| w.pr15().set_bit());
		}

		if pr.pr4().bit_is_set() {
			if ctx.shared.pin_cs.lock(|pin| pin.is_low().unwrap()) {
				// If incoming Chip Select is low, turn on the SPI engine
				ctx.shared.spi.lock(|s| s.enable());
			} else {
				// If incoming Chip Select is high, turn off the SPI engine
				ctx.shared.spi.lock(|s| s.disable());
			}
			// Clear the pending flag for this pin
			ctx.shared.exti.pr.write(|w| w.pr4().set_bit());
		}
	}

	/// This is the USART1 task.
	///
	/// It fires whenever there is new data received on USART1. We should flag to the host
	/// that data is available.
	#[task(binds = USART1, shared = [serial, msg_q_in])]
	fn usart1_interrupt(mut ctx: usart1_interrupt::Context) {
		// Reading the register clears the RX-Not-Empty-Interrupt flag.
		match ctx.shared.serial.read() {
			Ok(b) => {
				let _ = ctx
					.shared
					.msg_q_in
					.lock(|q| q.enqueue(Message::UartByte(b)));
			}
			_ => {}
		}
	}

	/// This is the SPI1 task.
	///
	/// It fires whenever there is new data received on SPI1. We should flag to the host
	/// that data is available.
	#[task(binds = SPI1, shared = [spi])]
	fn spi1_interrupt(mut ctx: spi1_interrupt::Context) {
		ctx.shared.spi.lock(|spi| {
			spi.handle_isr();
		});
	}

	/// This is the LED blink task.
	///
	/// This task is called periodically. We check whether the status LED is currently on or off,
	/// and set it to the opposite. This makes the LED blink.
	#[task(shared = [led_power, state_dc_power_enabled], local = [ led_state: bool = false ])]
	fn led_power_blink(ctx: led_power_blink::Context) {
		if *ctx.shared.state_dc_power_enabled == DcPowerState::Off {
			if *ctx.local.led_state {
				ctx.shared.led_power.set_low().unwrap();
				*ctx.local.led_state = false;
			} else {
				ctx.shared.led_power.set_high().unwrap();
				*ctx.local.led_state = true;
			}
			led_power_blink::spawn_after(LED_PERIOD_MS.millis()).unwrap();
		}
	}

	/// This task polls our power and reset buttons.
	///
	/// We poll them rather than setting up an interrupt as we need to debounce
	/// them, which involves waiting a short period and checking them again.
	/// Given that we have to do that, we might as well not bother with the
	/// interrupt.
	#[task(
		shared = [
			led_power, button_power, button_reset,
			state_dc_power_enabled, pin_sys_reset, pin_dc_on
		],
		local = [ press_button_power_short, press_button_power_long, press_button_reset_short ]
	)]
	fn button_poll(ctx: button_poll::Context) {
		// Poll buttons
		let pwr_pressed: bool = ctx.shared.button_power.is_low().unwrap();
		let rst_pressed: bool = ctx.shared.button_reset.is_low().unwrap();

		// Update state
		let pwr_short_edge = ctx.local.press_button_power_short.update(pwr_pressed);
		let pwr_long_edge = ctx.local.press_button_power_long.update(pwr_pressed);
		let rst_long_edge = ctx.local.press_button_reset_short.update(rst_pressed);

		defmt::trace!(
			"pwr/rst {}/{} {}",
			pwr_pressed,
			rst_pressed,
			match rst_long_edge {
				Some(debouncr::Edge::Rising) => "rising",
				Some(debouncr::Edge::Falling) => "falling",
				None => "-",
			}
		);

		// Dispatch event
		match (
			pwr_long_edge,
			pwr_short_edge,
			*ctx.shared.state_dc_power_enabled,
		) {
			(None, Some(debouncr::Edge::Rising), DcPowerState::Off) => {
				defmt::info!("Power button pressed whilst off.");
				// Button pressed - power on system
				*ctx.shared.state_dc_power_enabled = DcPowerState::Starting;
				ctx.shared.led_power.set_high().unwrap();
				defmt::info!("Power on!");
				ctx.shared.pin_dc_on.set_high().unwrap();
				// TODO: Start monitoring 3.3V and 5.0V rails here
				// TODO: Take system out of reset when 3.3V and 5.0V are good
				ctx.shared.pin_sys_reset.set_high().unwrap();
			}
			(None, Some(debouncr::Edge::Falling), DcPowerState::Starting) => {
				defmt::info!("Power button released.");
				// Button released after power on
				*ctx.shared.state_dc_power_enabled = DcPowerState::On;
			}
			(Some(debouncr::Edge::Rising), None, DcPowerState::On) => {
				defmt::info!("Power button held whilst on.");
				*ctx.shared.state_dc_power_enabled = DcPowerState::Off;
				ctx.shared.led_power.set_low().unwrap();
				defmt::info!("Power off!");
				ctx.shared.pin_sys_reset.set_low().unwrap();
				ctx.shared.pin_dc_on.set_low().unwrap();
				// Start LED blinking again
				led_power_blink::spawn().unwrap();
			}
			_ => {
				// Do nothing
			}
		}

		// Did reset get a long press?
		if let Some(debouncr::Edge::Rising) = rst_long_edge {
			// Is the board powered on? Don't do a reset if it's powered off.
			if *ctx.shared.state_dc_power_enabled == DcPowerState::On {
				defmt::info!("Reset!");
				ctx.shared.pin_sys_reset.set_low().unwrap();
				// Returns an error if it's already scheduled
				let _ = exit_reset::spawn_after(RESET_DURATION_MS.millis());
			}
		}

		// Re-schedule the timer interrupt
		button_poll::spawn_after(DEBOUNCE_POLL_INTERVAL_MS.millis()).unwrap();
	}

	/// Return the reset line high (inactive), but only if we're still powered on.
	#[task(shared = [pin_sys_reset, state_dc_power_enabled])]
	fn exit_reset(ctx: exit_reset::Context) {
		defmt::debug!("End reset");
		if *ctx.shared.state_dc_power_enabled == DcPowerState::On {
			ctx.shared.pin_sys_reset.set_high().unwrap();
		}
	}
}

// TODO: Pins we haven't used yet
// SPI pins
// spi_clk: gpioa.pa5.into_alternate_af0(cs),
// spi_cipo: gpioa.pa6.into_alternate_af0(cs),
// spi_copi: gpioa.pa7.into_alternate_af0(cs),
// I²C pins
// i2c_scl: gpiob.pb6.into_alternate_af4(cs),
// i2c_sda: gpiob.pb7.into_alternate_af4(cs),
